use std::fmt::Display;

use async_trait::async_trait;

use crate::{
    error::{DynError, ErrorKind, SimpleError},
    file_classification::FilesForProgress,
    PartitionInfo,
};
use data_types::ParquetFile;

use super::PostClassificationPartitionFilter;

#[derive(Debug)]
pub struct PossibleProgressFilter {
    max_parquet_bytes: usize,
}

impl PossibleProgressFilter {
    pub fn new(max_parquet_bytes: usize) -> Self {
        Self { max_parquet_bytes }
    }
}

impl Display for PossibleProgressFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "possible_progress")
    }
}

#[async_trait]
impl PostClassificationPartitionFilter for PossibleProgressFilter {
    async fn apply(
        &self,
        partition_info: &PartitionInfo,
        files_to_make_progress_on: &FilesForProgress,
        files_to_keep: &[ParquetFile],
    ) -> Result<bool, DynError> {
        if !files_to_make_progress_on.is_empty() {
            // There is some files to compact or split; we can make progress
            Ok(true)
        } else {
            // No files means the split_compact cannot find any reasonable set of files to make progress on
            for f in files_to_keep {
                if f.file_size_bytes >= self.max_parquet_bytes as i64 && f.min_time == f.max_time {
                    return Err(SimpleError::new(
                        ErrorKind::OutOfMemory,
                        format!(
                            "partition {} has overlapped files that exceed max compact size limit {}, \
                            and cannot be split because they cover a single ns of time {}.",
                            partition_info.partition_id, self.max_parquet_bytes, f.min_time.get(),
                        ),
                    )
                    .into());
                }
            }

            // We just didn't have anything to compact in this branch.
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::{
        error::ErrorKindExt,
        file_classification::{CompactReason, FilesToSplitOrCompact},
        test_utils::PartitionInfoBuilder,
    };
    use iox_tests::ParquetFileBuilder;

    use super::*;

    #[test]
    fn test_display() {
        assert_eq!(
            PossibleProgressFilter::new(10).to_string(),
            "possible_progress"
        );
    }

    #[tokio::test]
    async fn test_apply_empty_ok() {
        let filter = PossibleProgressFilter::new(10);
        let p_info = Arc::new(PartitionInfoBuilder::new().with_partition_id(1).build());

        assert!(!filter
            .apply(&p_info, &FilesForProgress::empty(), &[])
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn test_apply_empty() {
        let big_file = ParquetFileBuilder::new(1).with_file_size_bytes(11).build();

        let filter = PossibleProgressFilter::new(10);
        let p_info = Arc::new(PartitionInfoBuilder::new().with_partition_id(1).build());
        let err = filter
            .apply(&p_info, &FilesForProgress::empty(), &[big_file])
            .await
            .unwrap_err();
        assert_eq!(err.classify(), ErrorKind::OutOfMemory);
        assert_eq!(
            err.to_string(),
            "partition 1 has overlapped files that exceed max compact size limit 10, \
            and cannot be split because they cover a single ns of time 0."
        );
    }

    #[tokio::test]
    async fn test_apply_not_empty() {
        let filter = PossibleProgressFilter::new(10);
        let p_info = Arc::new(PartitionInfoBuilder::new().with_partition_id(1).build());
        let f1 = ParquetFileBuilder::new(1).with_file_size_bytes(7).build();
        let files_for_progress = FilesForProgress {
            upgrade: vec![],
            split_or_compact: FilesToSplitOrCompact::Compact(
                vec![f1],
                // This reason is arbitrary
                CompactReason::ManySmallFiles,
            ),
        };
        assert!(filter
            .apply(&p_info, &files_for_progress, &[])
            .await
            .unwrap());
    }
}
