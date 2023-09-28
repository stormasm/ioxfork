use std::fmt::{Debug, Display};

use data_types::{ParquetFile, TransitionPartitionId};

use crate::round_info::CompactType;
pub mod many_files;

pub trait RoundSplit: Debug + Display + Send + Sync {
    /// Split files into two buckets "now" and "later".
    ///
    /// All files belong to the same partition.
    ///
    /// - **now:** will be processed in this round
    /// - **later:** will be processed in the next round
    fn split(
        &self,
        files: Vec<ParquetFile>,
        op: CompactType,
        partition: TransitionPartitionId,
    ) -> (Vec<ParquetFile>, Vec<ParquetFile>);
}
