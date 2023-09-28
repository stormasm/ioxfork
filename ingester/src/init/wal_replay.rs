use std::fmt::Debug;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use data_types::{NamespaceId, PartitionKey, SequenceNumber, TableId};
use generated_types::influxdata::iox::wal::v1::sequenced_wal_op::Op;
use metric::U64Counter;
use mutable_batch_pb::decode::decode_database_batch;
use observability_deps::tracing::*;
use thiserror::Error;
use wal::{SegmentId, SequencedWalOp};

use crate::{
    dml_payload::write::{PartitionedData, TableData, WriteOperation},
    dml_payload::IngestOp,
    dml_sink::{DmlError, DmlSink},
    ingest_state::{IngestState, IngestStateError},
    partition_iter::PartitionIter,
    persist::{drain_buffer::persist_partitions, queue::PersistQueue},
};

/// This duration controls how long to wait between reads of the ingest state
/// when WAL op replay is blocked on an unhealthy ingest state.
const OP_REPLAY_BACKPRESSURE_WAIT_DURATION: Duration = Duration::from_millis(500);

/// Errors returned when replaying the write-ahead log.
#[derive(Debug, Error)]
pub enum WalReplayError {
    /// An error initialising a segment file reader.
    #[error("failed to open wal segment for replay: {0}")]
    OpenSegment(wal::Error),

    /// An error when attempting to read an entry from the WAL, including the
    /// highest sequence number observed for successfully replayed entries.
    #[error("failed to read wal entry: {0}")]
    ReadEntry(wal::Error, Option<SequenceNumber>),

    /// An error converting the WAL entry into a [`IngestOp`].
    #[error("failed converting wal entry to ingest operation: {0}")]
    MapToDml(#[from] mutable_batch_pb::decode::Error),

    /// A failure to apply a [`IngestOp`] from the WAL to the in-memory
    /// [`BufferTree`].
    ///
    /// [`BufferTree`]: crate::buffer_tree::BufferTree
    #[error("failed to apply op: {0}")]
    Apply(#[from] DmlError),
}

/// A type that can list, read & delete closed WAL segment files. This abstracts
/// away the type of segment reader to allow mocking.
#[async_trait]
pub trait WalReader: Debug + Send + Sync + 'static {
    /// A reader for a closed WAL segment.
    type SegmentReader: SegmentedWalOpBatchReader;

    /// Returns a reader for the closed wal segment specified.
    fn reader_for_closed_segment(&self, id: SegmentId) -> Result<Self::SegmentReader, wal::Error>;

    /// Lists the closed segments available for reading from the WAL as (id, size) tuples.
    fn closed_segments(&self) -> Vec<(SegmentId, u64)>;

    /// Deletes the closed segment specified.
    async fn delete(&self, id: SegmentId) -> Result<(), wal::Error>;
}

#[async_trait]
impl WalReader for Arc<wal::Wal> {
    type SegmentReader = wal::ClosedSegmentFileReader;

    fn reader_for_closed_segment(&self, id: SegmentId) -> Result<Self::SegmentReader, wal::Error> {
        wal::Wal::reader_for_segment(self, id)
    }

    fn closed_segments(&self) -> Vec<(SegmentId, u64)> {
        wal::Wal::closed_segments(self)
            .iter()
            .map(|s| (s.id(), s.size()))
            .collect()
    }

    async fn delete(&self, id: SegmentId) -> Result<(), wal::Error> {
        wal::Wal::delete(self, id).await
    }
}

/// A trait to associate a [`SegmentId`] with a WAL op batch reader
pub trait SegmentedWalOpBatchReader:
    Iterator<Item = Result<Vec<SequencedWalOp>, wal::Error>> + Send
{
    /// The ID of the segment file the entries in the reader are from
    fn id(&self) -> SegmentId;
}

/// Implement the trait for the [`wal::ClosedSegmentFileReader`]
impl SegmentedWalOpBatchReader for wal::ClosedSegmentFileReader {
    fn id(&self) -> SegmentId {
        self.id()
    }
}

// TODO: tolerate WAL replay errors
//
// https://github.com/influxdata/influxdb_iox/issues/6283

/// Replay all the entries in `wal` to `sink`, returning the maximum observed
/// [`SequenceNumber`].
pub async fn replay<W, T, P>(
    wal: &W,
    sink: &T,
    persist: P,
    ingest_state: Arc<IngestState>,
    metrics: &metric::Registry,
) -> Result<Option<SequenceNumber>, WalReplayError>
where
    W: WalReader,
    T: DmlSink + PartitionIter,
    P: PersistQueue + Clone,
{
    // Read the set of files to replay.
    //
    // The WAL yields files ordered from oldest to newest, ensuring the ordering
    // of this replay is correct.
    let files = wal.closed_segments();
    if files.is_empty() {
        info!("no wal replay files found");
        return Ok(None);
    }

    // Initialise metrics to track the progress of the WAL replay.
    //
    // The file count tracks the number of WAL files that have started
    // replaying, as opposed to finished replaying - this gives us the ability
    // to monitor WAL replays that hang or otherwise go wrong.
    let file_count_metric = metrics
        .register_metric::<U64Counter>(
            "ingester_wal_replay_files_started",
            "Number of WAL files that have started to be replayed",
        )
        .recorder(&[]);

    // This captures files that have been replayed, allowing us to have an
    // approximate diff for started vs finished
    let replayed_file_count_metric = metrics.register_metric::<U64Counter>(
        "ingester_wal_replay_files_finished",
        "Number of WAL files that have been replayed",
    );
    let file_count_success_metric = replayed_file_count_metric.recorder(&[("result", "success")]);
    let file_count_error_truncated_metric =
        replayed_file_count_metric.recorder(&[("result", "error"), ("reason", "truncated")]);

    let op_count_metric = metrics.register_metric::<U64Counter>(
        "ingester_wal_replay_ops",
        "Number of operations replayed from the WAL",
    );
    let ok_op_count_metric = op_count_metric.recorder(&[("outcome", "success")]);
    let empty_op_count_metric = op_count_metric.recorder(&[("outcome", "skipped_empty")]);

    let n_files = files.len();
    info!(n_files, "found wal files for replay");

    // Replay each file, keeping track of the last observed sequence number.
    //
    // Applying writes to the buffer can only happen monotonically and this is
    // enforced within the buffer.
    let mut max_sequence = None;
    for (index, file) in files.into_iter().enumerate() {
        // Map 0-based iter index to 1 based file count
        let file_number = index + 1;
        let (file_id, file_size) = (file.0, file.1);

        file_count_metric.inc(1);

        // Read the segment
        let reader = wal
            .reader_for_closed_segment(file_id)
            .map_err(WalReplayError::OpenSegment)?;

        // Emit a log entry so progress can be tracked (and a problematic file
        // be identified should an explosion happen during replay).
        info!(
            file_number,
            n_files,
            %file_id,
            size = file_size,
            "replaying wal file"
        );

        // Replay this segment file, tracking successful replay in the metric
        let replay_result = replay_file(
            reader,
            sink,
            &ok_op_count_metric,
            &empty_op_count_metric,
            &ingest_state,
        )
        .await;
        if replay_result.is_ok() {
            file_count_success_metric.inc(1);
        }

        match replay_result {
            Ok(seq @ Some(_)) => max_sequence = max_sequence.max(seq),
            Ok(None) => {
                // This file was empty and should be deleted.
                warn!(
                    file_number,
                    n_files,
                    %file_id ,
                    size = file_size,
                    "dropping empty wal segment",
                );

                // A failure to delete an empty file MUST not prevent WAL
                // replay from continuing.
                if let Err(error) = wal.delete(file_id).await {
                    error!(
                        file_number,
                        n_files,
                        %file_id,
                        size = file_size,
                        %error,
                        "error dropping empty wal segment",
                    );
                }

                continue;
            }
            // If the replay results in an underlying end of file error when
            // this is the most recent segment file, it indicates there was
            // a truncated write that never succeeded with an ACK.
            //
            // In this case we can log a warning, register it through metrics
            // and carry on as nothing can be done.
            Err(
                ref e @ WalReplayError::ReadEntry(
                    wal::Error::UnableToReadNextOps {
                        source: wal::blocking::ReaderError::UnableToReadData { source: ref io_err },
                    },
                    seq,
                ),
            ) if io_err.kind() == std::io::ErrorKind::UnexpectedEof && file_number == n_files => {
                max_sequence = max_sequence.max(seq);
                file_count_error_truncated_metric.inc(1);
                warn!(%e, %file_id, "detected truncated WAL write, ending replay for file early");
            }
            Err(e) => return Err(e),
        };

        info!(
            file_number,
            n_files,
            %file_id,
            size = file_size,
            "persisting wal segment data"
        );

        // Persist all the data that was replayed from the WAL segment.
        persist_partitions(sink.partition_iter(), &persist).await;

        // Drop the newly persisted data - it should not be replayed.
        wal.delete(file_id)
            .await
            .expect("failed to drop wal segment");

        info!(
            file_number,
            n_files,
            %file_id,
            size = file_size,
            "dropped persisted wal segment"
        );
    }

    info!(
        max_sequence_number = ?max_sequence,
        "wal replay complete"
    );

    Ok(max_sequence)
}

/// Replay the entries in `file`, applying them to `buffer`. Returns the
/// highest sequence number observed across the batches read from the file, or
/// [`None`] if there were no entries read.
///
/// # Warnings
///
/// This function relies on the [`wal::blocking::ReaderError::UnableToReadData`]
/// error sourced from an unexpected eof error to mean that there are no more
/// valid completed writes which can be read from the provided `batches` and
/// that it is safe to ignore them.
async fn replay_file<T, F>(
    file: F,
    sink: &T,
    ok_op_count_metric: &U64Counter,
    empty_op_count_metric: &U64Counter,
    ingest_state: &Arc<IngestState>,
) -> Result<Option<SequenceNumber>, WalReplayError>
where
    T: DmlSink,
    F: SegmentedWalOpBatchReader,
{
    let mut max_sequence = None;
    let start = Instant::now();
    let segment_id = file.id();

    for batch in file {
        let ops = batch.map_err(|e| WalReplayError::ReadEntry(e, max_sequence))?;

        for op in ops {
            let SequencedWalOp {
                table_write_sequence_numbers,
                op,
            } = op;

            let op = match op {
                Op::Write(w) => w,
                Op::Delete(_) => unreachable!(),
                Op::Persist(_) => unreachable!(),
            };

            let mut op_min_sequence_number: Option<SequenceNumber> = None;
            let mut op_max_sequence_number = None;

            // Reconstruct the ingest operation
            let batches = decode_database_batch(&op)?;
            let namespace_id = NamespaceId::new(op.database_id);
            let partition_key = PartitionKey::from(op.partition_key);

            if batches.is_empty() {
                warn!(?segment_id, %namespace_id, "encountered wal op batch containing no table data, skipping replay");
                empty_op_count_metric.inc(1);
                continue;
            }

            let op = WriteOperation::new(
                namespace_id,
                batches
                    .into_iter()
                    .map(|(k, v)| {
                        let table_id = TableId::new(k);
                        let sequence_number = SequenceNumber::new(
                            *table_write_sequence_numbers
                                .get(&table_id)
                                .expect("attempt to apply unsequenced wal op"),
                        );

                        max_sequence = max_sequence.max(Some(sequence_number));
                        op_min_sequence_number = op_min_sequence_number
                            .map(|prev_sequence_number| prev_sequence_number.min(sequence_number))
                            .or(Some(sequence_number));
                        op_max_sequence_number = op_max_sequence_number.max(Some(sequence_number));

                        (
                            table_id,
                            TableData::new(table_id, PartitionedData::new(sequence_number, v)),
                        )
                    })
                    .collect(),
                partition_key,
                // TODO: A tracing context should be added for WAL replay.
                None,
            );

            loop {
                match ingest_state.read_with_exceptions([IngestStateError::DiskFull]) {
                    Ok(_) => break,
                    Err(e) => {
                        warn!(
                            ingest_state_error=%e,
                            wait_duration=?OP_REPLAY_BACKPRESSURE_WAIT_DURATION,
                            "ingest state is unhealthy, waiting for ingest state to recover before replaying wal op",
                        );
                        tokio::time::sleep(OP_REPLAY_BACKPRESSURE_WAIT_DURATION).await;
                    }
                }
            }

            debug!(
                ?op,
                ?op_min_sequence_number,
                ?op_max_sequence_number,
                "apply wal op"
            );

            // Apply the operation to the provided DML sink
            sink.apply(IngestOp::Write(op))
                .await
                .map_err(Into::<DmlError>::into)?;

            ok_op_count_metric.inc(1);
        }
    }

    // This file is complete, return the last observed sequence
    // number.
    debug!(?segment_id, "wal file replayed in {:?}", start.elapsed());
    Ok(max_sequence)
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Arc};

    use assert_matches::assert_matches;
    use async_trait::async_trait;
    use hashbrown::HashSet;
    use itertools::Itertools;
    use metric::{assert_counter, Attributes};
    use parking_lot::Mutex;
    use test_helpers::timeout::FutureTimeout;
    use wal::Wal;

    use crate::{
        buffer_tree::partition::PartitionData,
        dml_payload::{encode::encode_write_op, IngestOp},
        dml_sink::mock_sink::MockDmlSink,
        persist::queue::mock::MockPersistQueue,
        test_util::{
            assert_write_ops_eq, make_multi_table_write_op, make_write_op, PartitionDataBuilder,
            ARBITRARY_NAMESPACE_ID, ARBITRARY_PARTITION_KEY, ARBITRARY_TABLE_ID,
            ARBITRARY_TABLE_NAME, ARBITRARY_TRANSITION_PARTITION_ID,
        },
        wal::wal_sink::{mock::MockUnbufferedWriteNotifier, WalSink},
    };

    use super::*;

    #[derive(Debug)]
    struct MockIter {
        sink: MockDmlSink,
        partitions: Vec<Arc<Mutex<PartitionData>>>,
    }

    impl PartitionIter for MockIter {
        fn partition_iter(&self) -> Box<dyn Iterator<Item = Arc<Mutex<PartitionData>>> + Send> {
            Box::new(self.partitions.clone().into_iter())
        }
    }

    #[async_trait]
    impl DmlSink for MockIter {
        type Error = <MockDmlSink as DmlSink>::Error;

        async fn apply(&self, op: IngestOp) -> Result<(), Self::Error> {
            self.sink.apply(op).await
        }
    }

    const ALTERNATIVE_TABLE_NAME: &str = "arán";

    #[tokio::test]
    async fn test_replay() {
        let dir = tempfile::tempdir().unwrap();

        // Generate the test ops that will be appended and read back
        let op1 = make_write_op(
            &ARBITRARY_PARTITION_KEY,
            ARBITRARY_NAMESPACE_ID,
            &ARBITRARY_TABLE_NAME,
            ARBITRARY_TABLE_ID,
            24,
            &format!(
                r#"{},region=Madrid temp=35 4242424242"#,
                &*ARBITRARY_TABLE_NAME
            ),
            None,
        );
        let op2 = make_write_op(
            &ARBITRARY_PARTITION_KEY,
            ARBITRARY_NAMESPACE_ID,
            &ARBITRARY_TABLE_NAME,
            ARBITRARY_TABLE_ID,
            25,
            &format!(
                r#"{},region=Asturias temp=25 4242424242"#,
                &*ARBITRARY_TABLE_NAME
            ),
            None,
        );

        // Add a write hitting multiple tables for good measure
        let op3 = make_multi_table_write_op(
            &ARBITRARY_PARTITION_KEY,
            ARBITRARY_NAMESPACE_ID,
            [
                (
                    ARBITRARY_TABLE_NAME.to_string().as_str(),
                    ARBITRARY_TABLE_ID,
                    SequenceNumber::new(42),
                ),
                (
                    ALTERNATIVE_TABLE_NAME,
                    TableId::new(ARBITRARY_TABLE_ID.get() + 1),
                    SequenceNumber::new(43),
                ),
            ]
            .into_iter(),
            // Overwrite op2
            &format!(
                r#"{},region=Asturias temp=15 4242424242
                {},region=Mayo temp=12 4242424242"#,
                &*ARBITRARY_TABLE_NAME, ALTERNATIVE_TABLE_NAME,
            ),
        );

        // Emulate a mid-write crash by inserting an op with no data
        let empty_op = WriteOperation::new_empty_invalid(
            ARBITRARY_NAMESPACE_ID,
            ARBITRARY_PARTITION_KEY.clone(),
        );

        // The write portion of this test.
        //
        // Write two ops, rotate the file twice (ensuring an empty file is
        // handled ok), write a third op and finally an empty op.
        {
            let inner = Arc::new(MockDmlSink::default().with_apply_return(vec![
                Ok(()),
                Ok(()),
                Ok(()),
                Ok(()),
            ]));
            let wal = Wal::new(dir.path())
                .await
                .expect("failed to initialise WAL");
            let notifier_handle = Arc::new(MockUnbufferedWriteNotifier::default());

            let wal_sink = WalSink::new(
                Arc::clone(&inner),
                Arc::clone(&wal),
                Arc::clone(&notifier_handle),
            );

            // Apply the first op through the decorator
            wal_sink
                .apply(IngestOp::Write(op1.clone()))
                .await
                .expect("wal should not error");
            // And the second op
            wal_sink
                .apply(IngestOp::Write(op2.clone()))
                .await
                .expect("wal should not error");

            // Rotate the log file
            wal.rotate().expect("failed to rotate WAL file");

            // Rotate the log file again, in order to create an empty segment and ensure
            // replay is tolerant to it
            wal.rotate().expect("failed to rotate WAL file");

            // Write the third op
            wal_sink
                .apply(IngestOp::Write(op3.clone()))
                .await
                .expect("wal should not error");

            // Write the empty op
            wal_sink
                .apply(IngestOp::Write(empty_op))
                .await
                .expect("wal should not error");

            // Assert the mock inner sink saw the calls
            assert_eq!(inner.get_calls().len(), 4);
        }

        // Reinitialise the WAL
        let wal = Wal::new(dir.path())
            .await
            .expect("failed to initialise WAL");

        // Must be 3 segments, 1 OK, 1 Empty and 1 with a normal op and blank op
        assert_eq!(wal.closed_segments().len(), 3);

        // Initialise the mock persist system
        let persist = Arc::new(MockPersistQueue::default());

        // Replay the results into a mock to capture the DmlWrites and returns
        // some dummy partitions when iterated over.
        let mock_sink = MockDmlSink::default().with_apply_return(vec![Ok(()), Ok(()), Ok(())]);
        let mut partition = PartitionDataBuilder::new().build();
        // Put at least one write into the buffer so it is a candidate for persistence
        partition
            .buffer_write(
                op1.tables()
                    .next()
                    .unwrap()
                    .1
                    .partitioned_data()
                    .data()
                    .clone(),
                SequenceNumber::new(1),
            )
            .unwrap();
        let mock_iter = MockIter {
            sink: mock_sink,
            partitions: vec![Arc::new(Mutex::new(partition))],
        };

        let ingest_state = Arc::new(IngestState::default());
        let metrics = metric::Registry::default();
        let max_sequence_number = replay(
            &wal,
            &mock_iter,
            Arc::clone(&persist),
            Arc::clone(&ingest_state),
            &metrics,
        )
        .with_timeout_panic(Duration::from_secs(2))
        .await
        .expect("failed to replay WAL");

        assert_eq!(max_sequence_number, Some(SequenceNumber::new(43)));

        // Assert the ops were pushed into the DmlSink exactly as generated,
        // barring the empty op which is skipped
        let ops = mock_iter.sink.get_calls();
        assert_matches!(
            &*ops,
            &[
                IngestOp::Write(ref w1),
                IngestOp::Write(ref w2),
                IngestOp::Write(ref w3),
            ] => {
                assert_write_ops_eq(w1.clone(), op1);
                assert_write_ops_eq(w2.clone(), op2);
                assert_write_ops_eq(w3.clone(), op3);
            }
        );

        // Ensure all partitions were persisted
        let calls = persist.calls();
        assert_matches!(&*calls, [p] => {
            assert_eq!(p.lock().partition_id(), &*ARBITRARY_TRANSITION_PARTITION_ID);
        });

        // Ensure there were no partition persist panics.
        Arc::try_unwrap(persist)
            .expect("should be no more refs")
            .join()
            .await;

        // Ensure the replayed segments were dropped, including the empty one
        let wal = Wal::new(dir.path())
            .await
            .expect("failed to initialise WAL");

        assert_eq!(wal.closed_segments().len(), 1);

        // Validate the expected metric values were populated.
        assert_counter!(
            metrics,
            U64Counter,
            "ingester_wal_replay_files_started",
            value = 3,
        );
        assert_counter!(
            metrics,
            U64Counter,
            "ingester_wal_replay_files_finished",
            labels = Attributes::from(&[("result", "success")]),
            value = 3,
        );
        assert_counter!(
            metrics,
            U64Counter,
            "ingester_wal_replay_files_finished",
            labels = Attributes::from(&[("result", "error"), ("reason", "truncated")]),
            value = 0,
        );
        assert_counter!(
            metrics,
            U64Counter,
            "ingester_wal_replay_ops",
            labels = Attributes::from(&[("outcome", "success")]),
            value = 3,
        );
        assert_counter!(
            metrics,
            U64Counter,
            "ingester_wal_replay_ops",
            labels = Attributes::from(&[("outcome", "skipped_empty")]),
            value = 1,
        );
    }

    #[derive(Debug)]
    struct MockWalReader {
        readers: Mutex<VecDeque<MockSegmentedWalOpBatchReader>>,
        closed_segment_ids: Mutex<HashSet<SegmentId>>,
    }

    impl MockWalReader {
        fn new(
            readers: impl IntoIterator<Item = MockSegmentedWalOpBatchReader>,
            closed_segment_ids: impl IntoIterator<Item = u64>,
        ) -> Self {
            Self {
                readers: Mutex::new(readers.into_iter().collect()),
                closed_segment_ids: Mutex::new(
                    closed_segment_ids.into_iter().map(SegmentId::new).collect(),
                ),
            }
        }
    }

    #[async_trait]
    impl WalReader for MockWalReader {
        type SegmentReader = MockSegmentedWalOpBatchReader;

        fn reader_for_closed_segment(
            &self,
            id: SegmentId,
        ) -> Result<Self::SegmentReader, wal::Error> {
            assert!(self.closed_segment_ids.lock().contains(&id));
            Ok(self.readers.lock().pop_front().expect("no reader"))
        }

        fn closed_segments(&self) -> Vec<(SegmentId, u64)> {
            self.closed_segment_ids
                .lock()
                .iter()
                .sorted()
                .map(|id| (*id, 1))
                .collect()
        }

        async fn delete(&self, id: SegmentId) -> Result<(), wal::Error> {
            assert!(self.closed_segment_ids.lock().remove(&id));
            Ok(())
        }
    }

    #[derive(Debug)]
    struct MockSegmentedWalOpBatchReader {
        id: SegmentId,
        entry_results: VecDeque<Result<Vec<SequencedWalOp>, wal::Error>>,
    }

    impl MockSegmentedWalOpBatchReader {
        fn new(id: SegmentId) -> Self {
            Self {
                id,
                entry_results: Default::default(),
            }
        }

        fn with_entry_results(
            mut self,
            entry_results: impl IntoIterator<Item = Result<Vec<SequencedWalOp>, wal::Error>>,
        ) -> Self {
            self.entry_results = entry_results.into_iter().collect();
            self
        }
    }

    impl Iterator for MockSegmentedWalOpBatchReader {
        type Item = Result<Vec<SequencedWalOp>, wal::Error>;

        fn next(&mut self) -> Option<Self::Item> {
            self.entry_results.pop_front()
        }
    }

    impl SegmentedWalOpBatchReader for MockSegmentedWalOpBatchReader {
        fn id(&self) -> wal::SegmentId {
            self.id
        }
    }

    fn arbitrary_sequenced_wal_op(id: SequenceNumber) -> SequencedWalOp {
        use generated_types::influxdata::iox::wal::v1::sequenced_wal_op::Op as WalOp;

        let op = make_write_op(
            &ARBITRARY_PARTITION_KEY,
            ARBITRARY_NAMESPACE_ID,
            &ARBITRARY_TABLE_NAME,
            ARBITRARY_TABLE_ID,
            id.get(),
            &format!(
                r#"{},region=Belfast temp=14,climate="wet" 4242424242"#,
                &*ARBITRARY_TABLE_NAME
            ),
            None,
        );

        SequencedWalOp {
            table_write_sequence_numbers: [(ARBITRARY_TABLE_ID, id.get())].into_iter().collect(),
            op: WalOp::Write(encode_write_op(ARBITRARY_NAMESPACE_ID, &op)),
        }
    }

    #[tokio::test]
    async fn test_replay_of_truncated_write_in_last_file() {
        let wal = MockWalReader::new(
            [
                MockSegmentedWalOpBatchReader::new(SegmentId::new(1)).with_entry_results([Ok(
                    vec![arbitrary_sequenced_wal_op(SequenceNumber::new(1))],
                )]),
                MockSegmentedWalOpBatchReader::new(SegmentId::new(2)).with_entry_results([Ok(
                    vec![arbitrary_sequenced_wal_op(SequenceNumber::new(2))],
                )]),
                MockSegmentedWalOpBatchReader::new(SegmentId::new(3)).with_entry_results([
                    Ok(vec![arbitrary_sequenced_wal_op(SequenceNumber::new(3))]),
                    Err(wal::Error::UnableToReadNextOps {
                        source: wal::blocking::ReaderError::UnableToReadData {
                            source: std::io::Error::new(
                                std::io::ErrorKind::UnexpectedEof,
                                "gremlins in the drive",
                            ),
                        },
                    }),
                ]),
            ],
            [1, 2, 3],
        );

        // Initialise the mock persist system
        let persist = Arc::new(MockPersistQueue::default());

        // Replay the results into a mock to capture the DmlWrites and returns
        // some dummy partitions when iterated over.
        let mock_sink = MockDmlSink::default().with_apply_return(vec![Ok(()), Ok(()), Ok(())]);
        let mock_iter = MockIter {
            sink: mock_sink,
            partitions: vec![],
        };
        let metrics = metric::Registry::default();

        let max_sequence_number = replay(
            &wal,
            &mock_iter,
            Arc::clone(&persist),
            Arc::new(IngestState::default()),
            &metrics,
        )
        .await
        .expect("failed to replay WAL")
        .expect("should receive max sequence number");
        assert_eq!(max_sequence_number, SequenceNumber::new(3));
        assert!(wal.closed_segment_ids.lock().is_empty());

        assert_counter!(
            metrics,
            U64Counter,
            "ingester_wal_replay_files_finished",
            labels = Attributes::from(&[("result", "success")]),
            value = 2,
        );
        assert_counter!(
            metrics,
            U64Counter,
            "ingester_wal_replay_files_finished",
            labels = Attributes::from(&[("result", "error"), ("reason", "truncated")]),
            value = 1,
        );
    }

    #[tokio::test]
    async fn test_replay_error_for_unknown_corruption() {
        let wal = MockWalReader::new(
            [
                MockSegmentedWalOpBatchReader::new(SegmentId::new(1)).with_entry_results([Ok(
                    vec![arbitrary_sequenced_wal_op(SequenceNumber::new(1))],
                )]),
                MockSegmentedWalOpBatchReader::new(SegmentId::new(2)).with_entry_results([
                    Ok(vec![arbitrary_sequenced_wal_op(SequenceNumber::new(2))]),
                    Err(wal::Error::UnableToReadNextOps {
                        source: wal::blocking::ReaderError::UnableToReadData {
                            source: std::io::Error::new(
                                std::io::ErrorKind::UnexpectedEof,
                                "gremlins in the drive",
                            ),
                        },
                    }),
                ]),
                MockSegmentedWalOpBatchReader::new(SegmentId::new(3)).with_entry_results([Ok(
                    vec![arbitrary_sequenced_wal_op(SequenceNumber::new(4))],
                )]),
            ],
            [1, 2, 3],
        );

        // Initialise the mock persist system
        let persist = Arc::new(MockPersistQueue::default());

        // Replay the results into a mock to capture the DmlWrites and returns
        // some dummy partitions when iterated over.
        let mock_sink = MockDmlSink::default().with_apply_return(vec![Ok(()), Ok(()), Ok(())]);
        let mock_iter = MockIter {
            sink: mock_sink,
            partitions: vec![],
        };
        let metrics = metric::Registry::default();

        let replay_result = replay(
            &wal,
            &mock_iter,
            Arc::clone(&persist),
            Arc::new(IngestState::default()),
            &metrics,
        )
        .await;
        assert_matches!(
            replay_result,
            Err(WalReplayError::ReadEntry(_, Some(id))) => {
                assert_eq!(id, SequenceNumber::new(2));
            }
        );
        assert_eq!(
            wal.closed_segments()
                .into_iter()
                .map(|s| s.0)
                .collect::<Vec<_>>(),
            vec![SegmentId::new(2), SegmentId::new(3)]
        );
    }

    #[tokio::test]
    async fn test_replay_respects_ingest_state() {
        let metrics = metric::Registry::default();
        let metric = metrics.register_metric::<U64Counter>("foo", "bar");
        let reader = MockSegmentedWalOpBatchReader::new(SegmentId::new(1)).with_entry_results([
            Ok(vec![arbitrary_sequenced_wal_op(SequenceNumber::new(1))]),
            Ok(vec![arbitrary_sequenced_wal_op(SequenceNumber::new(2))]),
        ]);
        let mock_sink = Arc::new(MockDmlSink::default().with_apply_return(vec![Ok(()), Ok(())]));
        // Create a blocked ingest state
        let ingest_state = Arc::new(IngestState::default());
        assert!(ingest_state.set(IngestStateError::PersistSaturated));

        // Kick off the replay task, which should block attempting to apply
        // any operations until the ingest state is healthy
        let replay_task = {
            let mock_sink = Arc::clone(&mock_sink);
            let ingest_state = Arc::clone(&ingest_state);

            tokio::spawn(async move {
                replay_file(
                    reader,
                    &mock_sink,
                    &metric.recorder(&[]),
                    &metric.recorder(&[]),
                    &ingest_state,
                )
                .await
            })
        };

        // Sleep the test thread to yield to the file replay and give it a
        // chance to spin, before ensuring the handle has not finished and
        // no writes were applied to the sink.
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(!replay_task.is_finished());
        assert!(mock_sink.get_calls().is_empty());

        // Unblock the ingest state and assert file replay proceeds to complete
        // with the sink having received the expected number of calls
        assert!(ingest_state.unset(IngestStateError::PersistSaturated));
        assert_matches!(replay_task
            .with_timeout_panic(Duration::from_secs(2))
            .await
            .expect("replay task failed to join"),
            Ok(Some(id)) => {
                assert_eq!(id, SequenceNumber::new(2));
            }
        );
        assert_eq!(mock_sink.get_calls().len(), 2);
    }

    #[tokio::test]
    async fn test_replay_continues_when_disk_full() {
        let metrics = metric::Registry::default();
        let metric = metrics.register_metric::<U64Counter>("foo", "bar");
        let reader = MockSegmentedWalOpBatchReader::new(SegmentId::new(1)).with_entry_results([
            Ok(vec![arbitrary_sequenced_wal_op(SequenceNumber::new(1))]),
            Ok(vec![arbitrary_sequenced_wal_op(SequenceNumber::new(2))]),
        ]);
        let mock_sink = MockDmlSink::default().with_apply_return(vec![Ok(()), Ok(())]);

        // Construct an IngestState with `DiskFull` and ensure that the file is replayed.
        let ingest_state = Arc::new(IngestState::default());
        ingest_state.set(IngestStateError::DiskFull);

        assert_matches!(
            replay_file(
                reader,
                &mock_sink,
                &metric.recorder(&[]),
                &metric.recorder(&[]),
                &Arc::clone(&ingest_state),
            )
            .with_timeout_panic(Duration::from_secs(2))
            .await,
            Ok(Some(id))=> {
                assert_eq!(id, SequenceNumber::new(2));
            }
        );
        assert_eq!(mock_sink.get_calls().len(), 2);
    }
}
