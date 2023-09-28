//! Partition level data buffer structures.

use std::sync::Arc;

use data_types::{
    sequence_number_set::SequenceNumberSet, NamespaceId, PartitionKey, SequenceNumber,
    SortedColumnSet, TableId, TimestampMinMax, TransitionPartitionId,
};
use mutable_batch::MutableBatch;
use observability_deps::tracing::*;
use schema::{merge::SchemaMerger, sort::SortKey, Schema};

use self::{
    buffer::{traits::Queryable, DataBuffer},
    counter::PartitionCounter,
    persisting::{BatchIdent, PersistingData},
    persisting_list::PersistingList,
};
use super::{namespace::NamespaceName, table::metadata::TableMetadata, BufferWriteError};
use crate::{
    deferred_load::DeferredLoad, query::projection::OwnedProjection, query_adaptor::QueryAdaptor,
};

mod buffer;
pub(crate) mod counter;
pub(crate) mod persisting;
mod persisting_list;
pub(crate) mod resolver;

/// The load state of the [`SortKey`] for a given partition.
#[derive(Debug, Clone)]
pub(crate) enum SortKeyState {
    /// The [`SortKey`] has not yet been fetched from the catalog, and will be
    /// lazy loaded (or loaded in the background) by a call to
    /// [`DeferredLoad::get()`].
    Deferred(Arc<DeferredLoad<(Option<SortKey>, Option<SortedColumnSet>)>>),
    /// The sort key is known and specified.
    Provided(Option<SortKey>, Option<SortedColumnSet>),
}

impl SortKeyState {
    pub(crate) async fn get_sort_key(&self) -> Option<SortKey> {
        match self {
            Self::Deferred(v) => v.get().await.0,
            Self::Provided(sort_key, _sort_key_ids) => sort_key.clone(),
        }
    }

    pub(crate) async fn get_sort_key_ids(&self) -> Option<SortedColumnSet> {
        match self {
            Self::Deferred(v) => v.get().await.1,
            Self::Provided(_sort_key, sort_key_ids) => sort_key_ids.clone(),
        }
    }

    // get both sort key and sort key ids
    pub(crate) async fn get(&self) -> (Option<SortKey>, Option<SortedColumnSet>) {
        match self {
            Self::Deferred(v) => v.get().await,
            Self::Provided(sort_key, sort_key_ids) => (sort_key.clone(), sort_key_ids.clone()),
        }
    }
}

/// Data of an IOx Partition of a given Table of a Namespace
#[derive(Debug)]
pub struct PartitionData {
    /// The partition this buffer is for.
    partition_id: TransitionPartitionId,

    /// The string partition key for this partition.
    partition_key: PartitionKey,

    /// The sort key of this partition.
    ///
    /// This can known, in which case this field will contain a
    /// [`SortKeyState::Provided`] with the [`SortKey`], or unknown with a value
    /// of [`SortKeyState::Deferred`] causing it to be loaded from the catalog
    /// (potentially) in the background or at read time.
    ///
    /// Callers should use [`Self::sort_key()`] to be abstracted away from these
    /// fetch details.
    sort_key: SortKeyState,

    /// The namespace this partition is part of.
    namespace_id: NamespaceId,
    /// The name of the namespace this partition is part of, potentially
    /// unresolved / deferred.
    namespace_name: Arc<DeferredLoad<NamespaceName>>,

    /// The catalog ID for the table this partition is part of.
    table_id: TableId,
    /// The catalog metadata for the table this partition is part of, potentially unresolved
    /// / deferred.
    table: Arc<DeferredLoad<TableMetadata>>,

    /// An optimised empty indicator that returns true when the `buffer` and
    /// `persisting` list are empty: a subsequent query will not return any
    /// rows.
    ///
    /// False when this partition contains data.
    is_empty: bool,

    /// A [`DataBuffer`] for incoming writes.
    buffer: DataBuffer,

    /// The currently persisting [`DataBuffer`] instances, if any.
    ///
    /// This queue is ordered from newest at the head, to oldest at the tail -
    /// forward iteration order matches write order.
    ///
    /// The [`BatchIdent`] is a generational counter that is used to tag each
    /// persisting with a unique, opaque identifier.
    persisting: PersistingList,

    /// The number of persist operations started over the lifetime of this
    /// [`PartitionData`].
    started_persistence_count: BatchIdent,

    /// The number of persist operations completed over the lifetime of this
    /// [`PartitionData`].
    completed_persistence_count: u64,

    /// A counter tracking the number of non-empty partitions per namespace.
    ///
    /// This value is incremented when this [`PartitionData`] transitions from
    /// empty, to non-empty during write buffering. Likewise the value is
    /// decremented when persistence is marked as complete and the persisted
    /// data is dropped, transitioning the [`PartitionData`] from non-empty to
    /// empty.
    partition_counter: Arc<PartitionCounter>,
}

impl PartitionData {
    /// Initialize a new partition data buffer
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        partition_id: TransitionPartitionId,
        partition_key: PartitionKey,
        namespace_id: NamespaceId,
        namespace_name: Arc<DeferredLoad<NamespaceName>>,
        table_id: TableId,
        table: Arc<DeferredLoad<TableMetadata>>,
        sort_key: SortKeyState,
        partition_counter: Arc<PartitionCounter>,
    ) -> Self {
        Self {
            partition_id,
            partition_key,
            sort_key,
            namespace_id,
            namespace_name,
            table_id,
            table,
            buffer: DataBuffer::default(),
            persisting: PersistingList::default(),
            started_persistence_count: BatchIdent::default(),
            completed_persistence_count: 0,
            partition_counter,
            is_empty: true,
        }
    }

    /// Buffer the given [`MutableBatch`] in memory.
    pub(crate) fn buffer_write(
        &mut self,
        mb: MutableBatch,
        sequence_number: SequenceNumber,
    ) -> Result<(), BufferWriteError> {
        if self.is_empty() {
            // This partition is transitioning from empty, to non-empty.
            //
            // Because non-empty partitions are bounded per namespace, a check
            // must be made to ensure accepting this write does not exceed the
            // (approximate) limit.
            self.partition_counter.inc()?;
            self.is_empty = false;
        }

        // Invariant: the non-empty partition counter is always >0 at this
        // point because this partition is non-empty.
        debug_assert_ne!(self.partition_counter.read(), 0);

        // Buffer the write.
        self.buffer.buffer_write(mb, sequence_number)?;

        // Invariant: if the partition contains a buffered write, it must report
        // non-empty.
        debug_assert!(!self.is_empty());

        trace!(
            namespace_id = %self.namespace_id,
            table_id = %self.table_id,
            table = %self.table,
            partition_id = %self.partition_id,
            partition_key = %self.partition_key,
            "buffered write"
        );

        Ok(())
    }

    /// Return an estimated cost of persisting the data buffered in this
    /// [`PartitionData`].
    pub(crate) fn persist_cost_estimate(&self) -> usize {
        self.buffer.persist_cost_estimate()
    }

    /// Returns the number of rows currently buffered in this [`PartitionData`].
    ///
    /// The returned value will always match the row count of the data returned
    /// by a subsequent call to [`PartitionData::get_query_data()`], without any
    /// column projection applied.
    ///
    /// This value is inclusive of "hot" buffered data, and all currently
    /// persisting data.
    ///
    /// This is an `O(n)` operation where `n` is the number of currently
    /// persisting batches, plus 1 for the "hot" buffer. Reading the row count
    /// of each batch is `O(1)`. This method is expected to be fast.
    pub(crate) fn rows(&self) -> usize {
        self.persisting.rows() + self.buffer.rows()
    }

    /// Returns true if this partition contains no data (buffered or
    /// persisting).
    pub(crate) fn is_empty(&self) -> bool {
        // Assert the optimised bool is in-sync with the actual state.
        debug_assert_eq!(
            self.persisting.is_empty() && self.buffer.rows() == 0,
            self.is_empty
        );

        // Optimisation: instead of inspecting the persisting list, and
        // inspecting the row count of the data buffer, the empty state can be
        // correctly and cheaply tracked separately.
        self.is_empty
    }

    /// Return the timestamp min/max values for the data contained within this
    /// [`PartitionData`].
    ///
    /// The returned value will always match the timestamp summary values of the
    /// data returned by a subsequent call to
    /// [`PartitionData::get_query_data()`] iff the projection provided to the
    /// call includes the timestamp column (returns pre-projection value).
    ///
    /// This value is inclusive of "hot" buffered data, and all currently
    /// persisting data.
    ///
    /// This is an `O(n)` operation where `n` is the number of currently
    /// persisting batches, plus 1 for the "hot" buffer, Reading the timestamp
    /// statistics for each batch is `O(1)`. This method is expected to be fast.
    pub(crate) fn timestamp_stats(&self) -> Option<TimestampMinMax> {
        self.persisting
            .timestamp_stats()
            .into_iter()
            .chain(self.buffer.timestamp_stats())
            .reduce(|acc, v| TimestampMinMax {
                min: acc.min.min(v.min),
                max: acc.max.max(v.max),
            })
    }

    /// Return the schema of the data currently buffered within this
    /// [`PartitionData`].
    ///
    /// This schema is not additive - it is the union of the individual schema
    /// batches currently buffered and as such columns are removed as the
    /// individual batches containing those columns are persisted and dropped.
    pub(crate) fn schema(&self) -> Option<Schema> {
        if self.is_empty() {
            return None;
        }

        Some(
            self.persisting
                .schema()
                .into_iter()
                .cloned()
                .chain(self.buffer.schema())
                .fold(SchemaMerger::new(), |acc, v| {
                    acc.merge(&v).expect("schemas are incompatible")
                })
                .build(),
        )
    }

    /// Return all data for this partition, ordered by the calls to
    /// [`PartitionData::buffer_write()`].
    pub(crate) fn get_query_data(&mut self, projection: &OwnedProjection) -> Option<QueryAdaptor> {
        // Extract the buffered data, if any.
        let buffered_data = self.buffer.get_query_data(projection);

        // Prepend any currently persisting batches.
        //
        // The persisting RecordBatch instances MUST be ordered before the
        // buffered data to preserve the ordering of writes such that updates to
        // existing rows materialise to the correct output.
        let data = self
            .persisting
            .get_query_data(projection)
            .chain(buffered_data)
            .collect::<Vec<_>>();

        trace!(
            namespace_id = %self.namespace_id,
            table_id = %self.table_id,
            table = %self.table,
            partition_id = %self.partition_id,
            partition_key = %self.partition_key,
            n_batches = data.len(),
            "read partition data"
        );

        if data.is_empty() {
            debug_assert_eq!(self.rows(), 0);
            return None;
        }

        // Invariant: the number of rows is always > 0 at this point.
        debug_assert_ne!(self.rows(), 0);

        // Invariant: the non-empty partition counter is always >0 at this
        // point because this partition is non-empty.
        debug_assert_ne!(self.partition_counter.read(), 0);

        // Construct the query adaptor over the partition data.
        //
        // `data` MUST contain at least one row, or the constructor panics. This
        // is upheld by the FSM, which ensures only non-empty snapshots /
        // RecordBatch are generated. Because `data` contains at least one
        // RecordBatch, this invariant holds.
        let q = QueryAdaptor::new(self.partition_id.clone(), data);

        // Invariant: the number of rows returned in a query MUST always match
        // the row count reported by the rows() method.
        //
        // The row count is never affected by projection.
        debug_assert_eq!(q.num_rows(), self.rows());

        // Invariant: the timestamp min/max MUST match the values reported by
        // timestamp_stats(), iff the projection contains the "time" column.
        #[cfg(debug_assertions)]
        {
            if projection
                .columns()
                .map(|v| v.iter().any(|v| v == schema::TIME_COLUMN_NAME))
                .unwrap_or(true)
            {
                assert_eq!(q.ts_min_max(), self.timestamp_stats());
            } else {
                // Otherwise the timestamp summary in the query response MUST be
                // empty.
                assert_eq!(q.ts_min_max(), None);
            }
        }

        Some(q)
    }

    /// Snapshot and mark all buffered data as persisting.
    ///
    /// This method returns [`None`] if no data is buffered in [`Self`].
    ///
    /// A reference to the persisting data is retained until a corresponding
    /// call to [`Self::mark_persisted()`] is made to release it.
    ///
    /// Additionally each persistence MAY update the partition sort key, which
    /// is not a commutative operations, requiring partition persistence to be
    /// serialised (unless it can be known in advance no sort key update is
    /// necessary for a given persistence).
    pub(crate) fn mark_persisting(&mut self) -> Option<PersistingData> {
        let fsm = std::mem::take(&mut self.buffer).into_persisting()?;

        // From this point on, all code MUST be infallible or the buffered data
        // contained within persisting may be dropped.

        // Invariant: the non-empty partition counter is always >0 at this
        // point because this partition is non-empty.
        assert!(self.partition_counter.read() > 0);

        // Increment the "started persist" counter.
        //
        // This is used to cheaply identify batches given to the
        // mark_persisted() call and ensure monotonicity.
        let batch_ident = self.started_persistence_count.next();

        debug!(
            namespace_id = %self.namespace_id,
            table_id = %self.table_id,
            table = %self.table,
            partition_id = %self.partition_id,
            partition_key = %self.partition_key,
            %batch_ident,
            "marking partition as persisting"
        );

        // Wrap the persisting data in the type wrapper
        let data = PersistingData::new(
            QueryAdaptor::new(
                self.partition_id.clone(),
                fsm.get_query_data(&OwnedProjection::default()),
            ),
            batch_ident,
        );

        // Push the buffer into the persisting list (which maintains batch
        // order).
        self.persisting.push(batch_ident, fsm);

        // Invariant: the partition must not be marked as empty when there's an
        // entry in the persisting list.
        debug_assert!(!self.is_empty());

        // Invariant: the number of rows is always > 0 at this point.
        debug_assert_ne!(self.rows(), 0);

        Some(data)
    }

    /// Mark this partition as having completed persistence of the specified
    /// `batch`.
    ///
    /// All internal references to the data in `batch` are released.
    ///
    /// # Panics
    ///
    /// This method panics if [`Self`] is not marked as undergoing a persist
    /// operation, or `batch` is not currently being persisted.
    pub(crate) fn mark_persisted(&mut self, batch: PersistingData) -> SequenceNumberSet {
        // Invariant: the partition must not be marked as empty when marking
        // persisting data as complete.
        debug_assert!(!self.is_empty());

        let fsm = self.persisting.remove(batch.batch_ident());

        self.completed_persistence_count += 1;

        debug!(
            persistence_count = %self.completed_persistence_count,
            namespace_id = %self.namespace_id,
            table_id = %self.table_id,
            table = %self.table,
            partition_id = %self.partition_id,
            partition_key = %self.partition_key,
            batch_ident = %batch.batch_ident(),
            "marking partition persistence complete"
        );

        // Invariant: the non-empty partition counter is always >0 at this
        // point because this partition is non-empty.
        assert!(self.partition_counter.read() > 0);

        // If this partition is now empty, the buffered partition counter should
        // be decremented to return the permit to the per-namespace pool.
        //
        // Check the actual partition data content (rather than relying on
        // is_empty bool) as the list was modified above and the bool state may
        // need updating.
        if self.persisting.is_empty() && self.buffer.rows() == 0 {
            // This partitioning from non-empty to empty.
            self.partition_counter.dec();
            self.is_empty = true;
        }

        // Return the set of IDs this buffer contained.
        fsm.into_sequence_number_set()
    }

    pub(crate) fn partition_id(&self) -> &TransitionPartitionId {
        &self.partition_id
    }

    /// Return the count of persisted Parquet files for this [`PartitionData`] instance.
    pub(crate) fn completed_persistence_count(&self) -> u64 {
        self.completed_persistence_count
    }

    /// Return the metadata of the table this [`PartitionData`] is buffering writes
    /// for.
    pub(crate) fn table(&self) -> &Arc<DeferredLoad<TableMetadata>> {
        &self.table
    }

    /// Return the table ID for this partition.
    pub(crate) fn table_id(&self) -> TableId {
        self.table_id
    }

    /// Return the partition key for this partition.
    pub(crate) fn partition_key(&self) -> &PartitionKey {
        &self.partition_key
    }

    /// Return the [`NamespaceId`] this partition is a part of.
    pub(crate) fn namespace_id(&self) -> NamespaceId {
        self.namespace_id
    }

    /// Return the [`NamespaceName`] this partition is a part of, potentially
    /// deferred / not yet resolved.
    ///
    /// NOTE: this MAY involve querying the catalog with unbounded retries.
    pub(crate) fn namespace_name(&self) -> &Arc<DeferredLoad<NamespaceName>> {
        &self.namespace_name
    }

    /// Return the [`SortKey`] for this partition.
    ///
    /// NOTE: this MAY involve querying the catalog with unbounded retries.
    pub(crate) fn sort_key(&self) -> &SortKeyState {
        &self.sort_key
    }

    /// Set the cached [`SortKey`] and its corresponding sort_key_ids to the specified values.
    ///
    /// All subsequent calls to [`Self::sort_key`] will return
    /// [`SortKeyState::Provided`]  with the `new`.
    pub(crate) fn update_sort_key(
        &mut self,
        new_sort_key: Option<SortKey>,
        new_sort_key_ids: SortedColumnSet,
    ) {
        assert!(new_sort_key.is_some());
        assert_eq!(new_sort_key.as_ref().unwrap().len(), new_sort_key_ids.len());
        self.sort_key = SortKeyState::Provided(new_sort_key, Some(new_sort_key_ids));
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use arrow::compute::SortOptions;
    use arrow_util::assert_batches_eq;
    use assert_matches::assert_matches;
    use backoff::BackoffConfig;
    use data_types::SortedColumnSet;
    use datafusion::{
        physical_expr::PhysicalSortExpr,
        physical_plan::{expressions::col, memory::MemoryExec, ExecutionPlan},
    };
    use datafusion_util::test_collect;
    use iox_catalog::interface::Catalog;
    use mutable_batch_lp::test_helpers::lp_to_mutable_batch;

    use super::*;
    use crate::{
        buffer_tree::partition::resolver::SortKeyResolver,
        test_util::{populate_catalog, PartitionDataBuilder, ARBITRARY_TRANSITION_PARTITION_ID},
    };

    // Write some data and read it back from the buffer.
    //
    // This ensures the sequence range, progress API, buffering, snapshot
    // generation & query all work as intended.
    #[tokio::test]
    async fn test_write_read() {
        let mut p = PartitionDataBuilder::new().build();

        // And no data should be returned when queried.
        assert!(p.get_query_data(&OwnedProjection::default()).is_none());

        // Perform a single write.
        let mb = lp_to_mutable_batch(r#"bananas,city=London people=2,pigeons="millions" 10"#).1;
        p.buffer_write(mb, SequenceNumber::new(1))
            .expect("write should succeed");

        // The data should be readable.
        {
            let data = p
                .get_query_data(&OwnedProjection::default())
                .expect("should return data");
            assert_eq!(data.partition_id(), &*ARBITRARY_TRANSITION_PARTITION_ID);

            let expected = [
                "+--------+--------+----------+--------------------------------+",
                "| city   | people | pigeons  | time                           |",
                "+--------+--------+----------+--------------------------------+",
                "| London | 2.0    | millions | 1970-01-01T00:00:00.000000010Z |",
                "+--------+--------+----------+--------------------------------+",
            ];
            assert_batches_eq!(expected, data.record_batches());
        }

        // Perform a another write, adding data to the existing queryable data
        // snapshot.
        let mb = lp_to_mutable_batch(r#"bananas,city=Madrid people=4,pigeons="none" 20"#).1;
        p.buffer_write(mb, SequenceNumber::new(2))
            .expect("write should succeed");

        // And finally both writes should be readable.
        {
            let data = p
                .get_query_data(&OwnedProjection::default())
                .expect("should contain data");
            assert_eq!(data.partition_id(), &*ARBITRARY_TRANSITION_PARTITION_ID);

            let expected = [
                "+--------+--------+----------+--------------------------------+",
                "| city   | people | pigeons  | time                           |",
                "+--------+--------+----------+--------------------------------+",
                "| London | 2.0    | millions | 1970-01-01T00:00:00.000000010Z |",
                "| Madrid | 4.0    | none     | 1970-01-01T00:00:00.000000020Z |",
                "+--------+--------+----------+--------------------------------+",
            ];
            assert_batches_eq!(expected, data.record_batches());
        }
    }

    // Test persist operations against the partition, ensuring data is readable
    // both before, during, and after a persist takes place.
    #[tokio::test]
    async fn test_persist() {
        let mut p = PartitionDataBuilder::new().build();

        assert!(p.get_query_data(&OwnedProjection::default()).is_none());

        // Perform a single write.
        let mb = lp_to_mutable_batch(r#"bananas,city=London people=2,pigeons="millions" 10"#).1;
        p.buffer_write(mb, SequenceNumber::new(1))
            .expect("write should succeed");

        // Ensure the batch ident hasn't been increased yet.
        assert_eq!(p.started_persistence_count.get(), 0);
        assert_eq!(p.completed_persistence_count, 0);

        // Begin persisting the partition.
        let persisting_data = p.mark_persisting().expect("must contain existing data");
        // And validate the data being persisted.
        assert_eq!(
            persisting_data.partition_id(),
            &*ARBITRARY_TRANSITION_PARTITION_ID
        );
        assert_eq!(persisting_data.record_batches().len(), 1);
        let expected = [
            "+--------+--------+----------+--------------------------------+",
            "| city   | people | pigeons  | time                           |",
            "+--------+--------+----------+--------------------------------+",
            "| London | 2.0    | millions | 1970-01-01T00:00:00.000000010Z |",
            "+--------+--------+----------+--------------------------------+",
        ];
        assert_batches_eq!(expected, persisting_data.record_batches());

        // Ensure the started batch ident is increased after a persist call, but not the completed
        // batch ident.
        assert_eq!(p.started_persistence_count.get(), 1);
        assert_eq!(p.completed_persistence_count, 0);
        // And the batch is correctly identified
        assert_eq!(persisting_data.batch_ident().get(), 1);

        // Buffer another write during an ongoing persist.
        let mb = lp_to_mutable_batch(r#"bananas,city=Madrid people=4,pigeons="none" 20"#).1;
        p.buffer_write(mb, SequenceNumber::new(2))
            .expect("write should succeed");

        // Which must be readable, alongside the ongoing persist data.
        {
            let data = p
                .get_query_data(&OwnedProjection::default())
                .expect("must have data");
            assert_eq!(data.partition_id(), &*ARBITRARY_TRANSITION_PARTITION_ID);
            assert_eq!(data.record_batches().len(), 2);
            let expected = [
                "+--------+--------+----------+--------------------------------+",
                "| city   | people | pigeons  | time                           |",
                "+--------+--------+----------+--------------------------------+",
                "| London | 2.0    | millions | 1970-01-01T00:00:00.000000010Z |",
                "| Madrid | 4.0    | none     | 1970-01-01T00:00:00.000000020Z |",
                "+--------+--------+----------+--------------------------------+",
            ];
            assert_batches_eq!(expected, data.record_batches());
        }

        // The persist now "completes".
        let set = p.mark_persisted(persisting_data);
        assert_eq!(set.len(), 1);
        assert!(set.contains(SequenceNumber::new(1)));

        // Ensure the started batch ident isn't increased when a persist completes, but the
        // completed count is increased.
        assert_eq!(p.started_persistence_count.get(), 1);
        assert_eq!(p.completed_persistence_count, 1);

        // Querying the buffer should now return only the second write.
        {
            let data = p
                .get_query_data(&OwnedProjection::default())
                .expect("must have data");
            assert_eq!(data.partition_id(), &*ARBITRARY_TRANSITION_PARTITION_ID);
            assert_eq!(data.record_batches().len(), 1);
            let expected = [
                "+--------+--------+---------+--------------------------------+",
                "| city   | people | pigeons | time                           |",
                "+--------+--------+---------+--------------------------------+",
                "| Madrid | 4.0    | none    | 1970-01-01T00:00:00.000000020Z |",
                "+--------+--------+---------+--------------------------------+",
            ];
            assert_batches_eq!(expected, data.record_batches());
        }

        assert!(!p.is_empty());

        // Persist the last buffered data, leaving the partition empty.
        let data = p.mark_persisting().expect("must contain existing data");
        assert_eq!(p.started_persistence_count.get(), 2);
        assert_eq!(p.completed_persistence_count, 1);

        let _ = p.mark_persisted(data);
        assert_eq!(p.started_persistence_count.get(), 2);
        assert_eq!(p.completed_persistence_count, 2);

        // Querying the buffer should now return no data
        assert!(p.get_query_data(&OwnedProjection::default()).is_none());
        assert!(p.is_empty());
    }

    // Ensure the ordering of snapshots & persisting data is preserved such that
    // updates resolve correctly, and batch identifiers are correctly allocated
    // and validated in mark_persisted() calls which return the correct
    // SequenceNumberSet instances.
    #[tokio::test]
    async fn test_record_batch_ordering() {
        // A helper function to dedupe the record batches in [`QueryAdaptor`]
        // and assert the resulting batch contents.
        async fn assert_deduped(expect: &[&str], batch: QueryAdaptor) {
            let batch = batch.record_batches().to_vec();

            let sort_keys = vec![PhysicalSortExpr {
                expr: col("time", &batch[0].schema()).unwrap(),
                options: SortOptions {
                    descending: false,
                    nulls_first: false,
                },
            }];

            // Setup in memory stream
            let schema = batch[0].schema();
            let projection = None;
            let input = Arc::new(MemoryExec::try_new(&[batch], schema, projection).unwrap());

            // Create and run the deduplicator
            let exec = Arc::new(iox_query::provider::DeduplicateExec::new(
                input, sort_keys, false,
            ));
            let got = test_collect(Arc::clone(&exec) as Arc<dyn ExecutionPlan>).await;

            assert_batches_eq!(expect, &*got);
        }

        let mut p = PartitionDataBuilder::new().build();

        // Perform the initial write.
        //
        // In the next series of writes this test will overwrite the value of x
        // and assert the deduped resulting state.
        let mb = lp_to_mutable_batch(r#"bananas x=1 42"#).1;
        p.buffer_write(mb, SequenceNumber::new(1))
            .expect("write should succeed");

        assert_eq!(
            p.get_query_data(&OwnedProjection::default())
                .unwrap()
                .record_batches()
                .len(),
            1
        );
        assert_deduped(
            &[
                "+--------------------------------+-----+",
                "| time                           | x   |",
                "+--------------------------------+-----+",
                "| 1970-01-01T00:00:00.000000042Z | 1.0 |",
                "+--------------------------------+-----+",
            ],
            p.get_query_data(&OwnedProjection::default()).unwrap(),
        )
        .await;

        // Write an update
        let mb = lp_to_mutable_batch(r#"bananas x=2 42"#).1;
        p.buffer_write(mb, SequenceNumber::new(2))
            .expect("write should succeed");

        assert_eq!(
            p.get_query_data(&OwnedProjection::default())
                .unwrap()
                .record_batches()
                .len(),
            1
        );
        assert_deduped(
            &[
                "+--------------------------------+-----+",
                "| time                           | x   |",
                "+--------------------------------+-----+",
                "| 1970-01-01T00:00:00.000000042Z | 2.0 |",
                "+--------------------------------+-----+",
            ],
            p.get_query_data(&OwnedProjection::default()).unwrap(),
        )
        .await;

        // Ensure the batch ident is increased after a persist call.
        assert_eq!(p.started_persistence_count.get(), 0);

        // Begin persisting the data, moving the buffer to the persisting state.

        let persisting_data1 = p.mark_persisting().unwrap();
        assert_eq!(persisting_data1.record_batches().len(), 1);
        assert_deduped(
            &[
                "+--------------------------------+-----+",
                "| time                           | x   |",
                "+--------------------------------+-----+",
                "| 1970-01-01T00:00:00.000000042Z | 2.0 |",
                "+--------------------------------+-----+",
            ],
            (*persisting_data1).clone(),
        )
        .await;

        // Ensure the batch ident is increased after a persist call.
        assert_eq!(p.started_persistence_count.get(), 1);
        // And the batch is correctly identified
        assert_eq!(persisting_data1.batch_ident().get(), 1);

        // Buffer another write, and generate a snapshot by querying it.
        let mb = lp_to_mutable_batch(r#"bananas x=3 42"#).1;
        p.buffer_write(mb, SequenceNumber::new(3))
            .expect("write should succeed");

        assert_eq!(
            p.get_query_data(&OwnedProjection::default())
                .unwrap()
                .record_batches()
                .len(),
            2
        );
        assert_deduped(
            &[
                "+--------------------------------+-----+",
                "| time                           | x   |",
                "+--------------------------------+-----+",
                "| 1970-01-01T00:00:00.000000042Z | 3.0 |",
                "+--------------------------------+-----+",
            ],
            p.get_query_data(&OwnedProjection::default()).unwrap(),
        )
        .await;

        // Persist again, moving the last write to the persisting state and
        // adding it to the persisting queue.

        let persisting_data2 = p.mark_persisting().unwrap();
        assert_eq!(persisting_data2.record_batches().len(), 1);
        assert_deduped(
            &[
                "+--------------------------------+-----+",
                "| time                           | x   |",
                "+--------------------------------+-----+",
                "| 1970-01-01T00:00:00.000000042Z | 3.0 |",
                "+--------------------------------+-----+",
            ],
            (*persisting_data2).clone(),
        )
        .await;

        // Ensure the batch ident is increased after a persist call.
        assert_eq!(p.started_persistence_count.get(), 2);
        // And the batch is correctly identified
        assert_eq!(persisting_data1.batch_ident().get(), 1);
        assert_eq!(persisting_data2.batch_ident().get(), 2);

        // Buffer another write, and generate a snapshot by querying it.
        let mb = lp_to_mutable_batch(r#"bananas x=4 42"#).1;
        p.buffer_write(mb, SequenceNumber::new(3))
            .expect("write should succeed");

        assert_eq!(
            p.get_query_data(&OwnedProjection::default())
                .unwrap()
                .record_batches()
                .len(),
            3
        );
        assert_deduped(
            &[
                "+--------------------------------+-----+",
                "| time                           | x   |",
                "+--------------------------------+-----+",
                "| 1970-01-01T00:00:00.000000042Z | 4.0 |",
                "+--------------------------------+-----+",
            ],
            p.get_query_data(&OwnedProjection::default()).unwrap(),
        )
        .await;

        // Finish persisting the first batch.
        let set = p.mark_persisted(persisting_data1);
        assert_eq!(set.len(), 2);
        assert!(set.contains(SequenceNumber::new(1)));
        assert!(set.contains(SequenceNumber::new(2)));

        // And assert the correct value remains.
        assert_eq!(
            p.get_query_data(&OwnedProjection::default())
                .unwrap()
                .record_batches()
                .len(),
            2
        );
        assert_deduped(
            &[
                "+--------------------------------+-----+",
                "| time                           | x   |",
                "+--------------------------------+-----+",
                "| 1970-01-01T00:00:00.000000042Z | 4.0 |",
                "+--------------------------------+-----+",
            ],
            p.get_query_data(&OwnedProjection::default()).unwrap(),
        )
        .await;

        // Finish persisting the second batch.
        let set = p.mark_persisted(persisting_data2);
        assert_eq!(set.len(), 1);
        assert!(set.contains(SequenceNumber::new(3)));

        // And assert the correct value remains.
        assert_eq!(
            p.get_query_data(&OwnedProjection::default())
                .unwrap()
                .record_batches()
                .len(),
            1
        );
        assert_deduped(
            &[
                "+--------------------------------+-----+",
                "| time                           | x   |",
                "+--------------------------------+-----+",
                "| 1970-01-01T00:00:00.000000042Z | 4.0 |",
                "+--------------------------------+-----+",
            ],
            p.get_query_data(&OwnedProjection::default()).unwrap(),
        )
        .await;

        assert_eq!(p.started_persistence_count.get(), 2);
    }

    // Ensure the ordering of snapshots & persisting data is preserved such that
    // updates resolve correctly when queried, and batch identifiers are
    // correctly allocated, validated, and removed in mark_persisted() calls
    // which return the correct SequenceNumberSet instances.
    #[tokio::test]
    async fn test_out_of_order_persist() {
        let mut p = PartitionDataBuilder::new().build();

        assert!(p.is_empty());

        // Perform the initial write.
        //
        // In the next series of writes this test will overwrite the value of x
        // and assert the deduped resulting state.
        let mb = lp_to_mutable_batch(r#"bananas x=1 42"#).1;
        p.buffer_write(mb, SequenceNumber::new(1))
            .expect("write should succeed");

        // Now contains data.
        assert!(!p.is_empty());

        // Begin persisting the data, moving the buffer to the persisting state.

        let persisting_data1 = p.mark_persisting().unwrap();

        // Remains non-empty until persisted.
        assert!(!p.is_empty());

        // Buffer another write, and generate a snapshot by querying it.
        let mb = lp_to_mutable_batch(r#"bananas x=2 42"#).1;
        p.buffer_write(mb, SequenceNumber::new(3))
            .expect("write should succeed");

        let data = p.get_query_data(&OwnedProjection::default()).unwrap();
        assert_batches_eq!(
            [
                "+--------------------------------+-----+",
                "| time                           | x   |",
                "+--------------------------------+-----+",
                "| 1970-01-01T00:00:00.000000042Z | 1.0 |",
                "| 1970-01-01T00:00:00.000000042Z | 2.0 |",
                "+--------------------------------+-----+",
            ],
            &*data.record_batches().to_vec()
        );

        // Persist again, moving the last write to the persisting state and
        // adding it to the persisting queue.

        let persisting_data2 = p.mark_persisting().unwrap();

        // Buffer another write, and generate a snapshot by querying it.
        let mb = lp_to_mutable_batch(r#"bananas x=3 42"#).1;
        p.buffer_write(mb, SequenceNumber::new(4))
            .expect("write should succeed");

        let data = p.get_query_data(&OwnedProjection::default()).unwrap();
        assert_batches_eq!(
            [
                "+--------------------------------+-----+",
                "| time                           | x   |",
                "+--------------------------------+-----+",
                "| 1970-01-01T00:00:00.000000042Z | 1.0 |",
                "| 1970-01-01T00:00:00.000000042Z | 2.0 |",
                "| 1970-01-01T00:00:00.000000042Z | 3.0 |",
                "+--------------------------------+-----+",
            ],
            &*data.record_batches().to_vec()
        );

        // Persist again, moving the last write to the persisting state and
        // adding it to the persisting queue ordered such that querying returns
        // the correctly ordered rows (newest rows last).
        let persisting_data3 = p.mark_persisting().unwrap();

        // Buffer another write, and generate a snapshot by querying it.
        let mb = lp_to_mutable_batch(r#"bananas x=4 42"#).1;
        p.buffer_write(mb, SequenceNumber::new(5))
            .expect("write should succeed");

        let data = p.get_query_data(&OwnedProjection::default()).unwrap();
        assert_batches_eq!(
            [
                "+--------------------------------+-----+",
                "| time                           | x   |",
                "+--------------------------------+-----+",
                "| 1970-01-01T00:00:00.000000042Z | 1.0 |",
                "| 1970-01-01T00:00:00.000000042Z | 2.0 |",
                "| 1970-01-01T00:00:00.000000042Z | 3.0 |",
                "| 1970-01-01T00:00:00.000000042Z | 4.0 |",
                "+--------------------------------+-----+",
            ],
            &*data.record_batches().to_vec()
        );

        // Finish persisting the second batch out-of-order! The middle entry,
        // ensuring the first and last entries remain ordered.
        let set = p.mark_persisted(persisting_data2);
        assert_eq!(set.len(), 1);
        assert!(set.contains(SequenceNumber::new(3)));

        let data = p.get_query_data(&OwnedProjection::default()).unwrap();
        assert_batches_eq!(
            [
                "+--------------------------------+-----+",
                "| time                           | x   |",
                "+--------------------------------+-----+",
                "| 1970-01-01T00:00:00.000000042Z | 1.0 |",
                "| 1970-01-01T00:00:00.000000042Z | 3.0 |",
                "| 1970-01-01T00:00:00.000000042Z | 4.0 |",
                "+--------------------------------+-----+",
            ],
            &*data.record_batches().to_vec()
        );

        // Finish persisting the last batch.
        let set = p.mark_persisted(persisting_data3);
        assert_eq!(set.len(), 1);
        assert!(set.contains(SequenceNumber::new(4)));

        let data = p.get_query_data(&OwnedProjection::default()).unwrap();
        assert_batches_eq!(
            [
                "+--------------------------------+-----+",
                "| time                           | x   |",
                "+--------------------------------+-----+",
                "| 1970-01-01T00:00:00.000000042Z | 1.0 |",
                "| 1970-01-01T00:00:00.000000042Z | 4.0 |",
                "+--------------------------------+-----+",
            ],
            &*data.record_batches().to_vec()
        );

        // Finish persisting the first batch.
        let set = p.mark_persisted(persisting_data1);
        assert_eq!(set.len(), 1);
        assert!(set.contains(SequenceNumber::new(1)));

        // Assert only the buffered data remains
        let data = p.get_query_data(&OwnedProjection::default()).unwrap();
        assert_batches_eq!(
            [
                "+--------------------------------+-----+",
                "| time                           | x   |",
                "+--------------------------------+-----+",
                "| 1970-01-01T00:00:00.000000042Z | 4.0 |",
                "+--------------------------------+-----+",
            ],
            &*data.record_batches().to_vec()
        );
    }

    // Ensure an updated sort key is returned.
    #[tokio::test]
    async fn test_update_provided_sort_key() {
        let starting_state = SortKeyState::Provided(
            Some(SortKey::from_columns(["banana", "time"])),
            Some(SortedColumnSet::from([1, 2])),
        );

        let mut p = PartitionDataBuilder::new()
            .with_sort_key_state(starting_state)
            .build();

        let want_sort_key = SortKey::from_columns(["banana", "platanos", "time"]);
        let want_sort_key_ids = SortedColumnSet::from([1, 3, 2]);
        p.update_sort_key(Some(want_sort_key.clone()), want_sort_key_ids.clone());

        assert_matches!(p.sort_key(), SortKeyState::Provided(_, _));
        assert_eq!(p.sort_key().get_sort_key().await.unwrap(), want_sort_key);
        assert_eq!(
            p.sort_key().get_sort_key_ids().await.unwrap(),
            want_sort_key_ids
        );
    }

    // Test loading a deferred sort key from the catalog on demand.
    #[tokio::test]
    async fn test_update_deferred_sort_key() {
        let metrics = Arc::new(metric::Registry::default());
        let backoff_config = BackoffConfig::default();
        let catalog: Arc<dyn Catalog> =
            Arc::new(iox_catalog::mem::MemCatalog::new(Arc::clone(&metrics)));

        let partition_key = PartitionKey::from("test");

        // Populate the catalog with the namespace / table
        let (_ns_id, table_id) = populate_catalog(&*catalog, "bananas", "platanos").await;

        let partition = catalog
            .repositories()
            .await
            .partitions()
            .create_or_get(partition_key.clone(), table_id)
            .await
            .expect("should create");
        // Test: sort_key_ids from create_or_get which is empty
        assert!(partition.sort_key_ids().is_empty());

        let updated_partition = catalog
            .repositories()
            .await
            .partitions()
            .cas_sort_key(
                &partition.transition_partition_id(),
                None,
                None,
                &["terrific"],
                &SortedColumnSet::from([1]),
            )
            .await
            .unwrap();
        // Test: sort_key_ids after updating
        assert_eq!(
            updated_partition.sort_key_ids(),
            &SortedColumnSet::from([1])
        );

        // Read the just-created sort key (None)
        let fetcher = Arc::new(DeferredLoad::new(
            Duration::from_nanos(1),
            SortKeyResolver::new(
                partition_key.clone(),
                table_id,
                Arc::clone(&catalog),
                backoff_config.clone(),
            )
            .fetch(),
            &metrics,
        ));

        let starting_state = SortKeyState::Deferred(fetcher);

        let mut p = PartitionDataBuilder::new()
            .with_sort_key_state(starting_state)
            .build();

        let want_sort_key = SortKey::from_columns(["banana", "platanos", "time"]);
        let want_sort_key_ids = SortedColumnSet::from([1, 3, 2]);
        p.update_sort_key(Some(want_sort_key.clone()), want_sort_key_ids.clone());

        assert_matches!(p.sort_key(), SortKeyState::Provided(_, _));
        assert_eq!(p.sort_key().get_sort_key().await.unwrap(), want_sort_key);
        assert_eq!(
            p.sort_key().get_sort_key_ids().await.unwrap(),
            want_sort_key_ids
        );
    }

    // Perform writes with non-monotonic sequence numbers.
    #[tokio::test]
    async fn test_non_monotonic_writes() {
        let mut p = PartitionDataBuilder::new().build();

        // Perform out of order writes.
        p.buffer_write(
            lp_to_mutable_batch(r#"bananas,city=London people=2,pigeons="millions" 10"#).1,
            SequenceNumber::new(2),
        )
        .expect("write should succeed");
        let _ = p.buffer_write(
            lp_to_mutable_batch(r#"bananas,city=Madrid people=2,pigeons="none" 11"#).1,
            SequenceNumber::new(1),
        );

        // Nothing should explode, data should be readable.
        let data = p.get_query_data(&OwnedProjection::default()).unwrap();
        assert_batches_eq!(
            [
                "+--------+--------+----------+--------------------------------+",
                "| city   | people | pigeons  | time                           |",
                "+--------+--------+----------+--------------------------------+",
                "| London | 2.0    | millions | 1970-01-01T00:00:00.000000010Z |",
                "| Madrid | 2.0    | none     | 1970-01-01T00:00:00.000000011Z |",
                "+--------+--------+----------+--------------------------------+",
            ],
            &*data.record_batches().to_vec()
        );
    }

    #[tokio::test]
    async fn test_mark_persisting_no_data() {
        let mut p = PartitionDataBuilder::new().build();

        assert!(p.mark_persisting().is_none());
        assert!(p.is_empty());
    }

    #[tokio::test]
    async fn test_mark_persisting_twice() {
        let mut p = PartitionDataBuilder::new().build();

        let mb = lp_to_mutable_batch(r#"bananas,city=London people=2,pigeons="millions" 10"#).1;
        p.buffer_write(mb, SequenceNumber::new(2))
            .expect("write should succeed");

        assert!(p.mark_persisting().is_some());
        assert!(p.mark_persisting().is_none());
    }

    // Ensure an empty PartitionData does not panic due to constructing an empty
    // QueryAdaptor.
    #[tokio::test]
    async fn test_empty_partition_no_queryadaptor_panic() {
        let mut p = PartitionDataBuilder::new().build();

        assert!(p.get_query_data(&OwnedProjection::default()).is_none());
        assert!(p.is_empty());
    }
}
