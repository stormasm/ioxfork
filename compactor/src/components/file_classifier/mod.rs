use std::{
    fmt::{Debug, Display},
    sync::Arc,
};

use data_types::ParquetFile;

use crate::{
    file_classification::FileClassification, partition_info::PartitionInfo, round_info::CompactType,
};

pub mod logging;
pub mod split_based;

pub trait FileClassifier: Debug + Display + Send + Sync {
    fn classify(
        &self,
        partition_info: &PartitionInfo,
        op: &CompactType,
        files: Vec<ParquetFile>,
    ) -> FileClassification;
}

impl<T> FileClassifier for Arc<T>
where
    T: FileClassifier + ?Sized,
{
    fn classify(
        &self,
        partition_info: &PartitionInfo,
        op: &CompactType,
        files: Vec<ParquetFile>,
    ) -> FileClassification {
        self.as_ref().classify(partition_info, op, files)
    }
}
