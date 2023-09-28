use std::fmt::Display;

use async_trait::async_trait;
use data_types::ParquetFile;
use metric::{Registry, U64Counter};

use crate::{error::DynError, PartitionInfo};

use super::PartitionFilter;

const METRIC_NAME_PARTITION_FILTER_COUNT: &str = "iox_compactor_partition_filter_count";

#[derive(Debug)]
pub struct MetricsPartitionFilterWrapper<T>
where
    T: PartitionFilter,
{
    pass_counter: U64Counter,
    filter_counter: U64Counter,
    error_counter: U64Counter,
    inner: T,
    filter_type: &'static str,
}

impl<T> MetricsPartitionFilterWrapper<T>
where
    T: PartitionFilter,
{
    pub fn new(inner: T, registry: &Registry, filter_type: &'static str) -> Self {
        let metric = registry.register_metric::<U64Counter>(
            METRIC_NAME_PARTITION_FILTER_COUNT,
            "Number of times the compactor fetched fresh partitions",
        );

        let pass_counter = metric.recorder(&[("result", "pass"), ("filter_type", filter_type)]);
        let filter_counter = metric.recorder(&[("result", "filter"), ("filter_type", filter_type)]);
        let error_counter = metric.recorder(&[("result", "error"), ("filter_type", filter_type)]);

        Self {
            pass_counter,
            filter_counter,
            error_counter,
            inner,
            filter_type,
        }
    }
}

impl<T> Display for MetricsPartitionFilterWrapper<T>
where
    T: PartitionFilter,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "metrics({}, {})", self.inner, self.filter_type)
    }
}

#[async_trait]
impl<T> PartitionFilter for MetricsPartitionFilterWrapper<T>
where
    T: PartitionFilter,
{
    async fn apply(
        &self,
        partition_info: &PartitionInfo,
        files: &[ParquetFile],
    ) -> Result<bool, DynError> {
        let res = self.inner.apply(partition_info, files).await;
        match res {
            Ok(true) => {
                self.pass_counter.inc(1);
            }
            Ok(false) => {
                self.filter_counter.inc(1);
            }
            Err(_) => {
                self.error_counter.inc(1);
            }
        }
        res
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use metric::{assert_counter, Attributes};

    use crate::{
        components::partition_filter::has_files::HasFilesPartitionFilter,
        test_utils::PartitionInfoBuilder,
    };
    use iox_tests::ParquetFileBuilder;

    use super::*;

    #[test]
    fn test_display() {
        let registry = Registry::new();
        let filter =
            MetricsPartitionFilterWrapper::new(HasFilesPartitionFilter::new(), &registry, "test");
        assert_eq!(filter.to_string(), "metrics(has_files, test)",);
    }

    #[tokio::test]
    async fn test_apply() {
        let registry = Registry::new();
        let filter =
            MetricsPartitionFilterWrapper::new(HasFilesPartitionFilter::new(), &registry, "test");
        let p_info = Arc::new(PartitionInfoBuilder::new().with_partition_id(1).build());
        let f = ParquetFileBuilder::new(0).build();

        assert_pass_counter(&registry, 0);
        assert_filter_counter(&registry, 0);
        assert_error_counter(&registry, 0);

        assert!(!filter.apply(&p_info, &[]).await.unwrap());
        assert!(!filter.apply(&p_info, &[]).await.unwrap());
        assert!(filter.apply(&p_info, &[f]).await.unwrap());

        assert_pass_counter(&registry, 1);
        assert_filter_counter(&registry, 2);
        assert_error_counter(&registry, 0);
    }

    fn assert_pass_counter(registry: &Registry, value: u64) {
        assert_counter!(
            registry,
            U64Counter,
            METRIC_NAME_PARTITION_FILTER_COUNT,
            labels = Attributes::from(&[("result", "pass"), ("filter_type", "test")]),
            value = value,
        );
    }

    fn assert_filter_counter(registry: &Registry, value: u64) {
        assert_counter!(
            registry,
            U64Counter,
            METRIC_NAME_PARTITION_FILTER_COUNT,
            labels = Attributes::from(&[("result", "filter"), ("filter_type", "test")]),
            value = value,
        );
    }

    fn assert_error_counter(registry: &Registry, value: u64) {
        assert_counter!(
            registry,
            U64Counter,
            METRIC_NAME_PARTITION_FILTER_COUNT,
            labels = Attributes::from(&[("result", "error"), ("filter_type", "test")]),
            value = value,
        );
    }
}
