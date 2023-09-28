//! Tools to set up DataFusion statistics.

use std::{collections::HashMap, sync::Arc};

use data_types::TimestampMinMax;
use datafusion::{
    physical_plan::{ColumnStatistics, Statistics},
    scalar::ScalarValue,
};
use schema::{InfluxColumnType, Schema, TIME_DATA_TIMEZONE};

/// Represent known min/max values for a specific column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnRange {
    pub min_value: Arc<ScalarValue>,
    pub max_value: Arc<ScalarValue>,
}

/// Represents the known min/max values for a subset (not all) of the columns in a partition.
///
/// The values may not actually in any row.
///
/// These ranges apply to ALL rows (esp. in ALL files and ingester chunks) within in given partition.
pub type ColumnRanges = Arc<HashMap<Arc<str>, ColumnRange>>;

/// Create chunk [statistics](Statistics).
pub fn create_chunk_statistics(
    row_count: Option<usize>,
    schema: &Schema,
    ts_min_max: Option<TimestampMinMax>,
    ranges: Option<&ColumnRanges>,
) -> Statistics {
    let mut columns = Vec::with_capacity(schema.len());

    for (t, field) in schema.iter() {
        let stats = match t {
            InfluxColumnType::Timestamp => {
                // prefer explicitely given time range but fall back to column ranges
                let (min_value, max_value) = match ts_min_max {
                    Some(ts_min_max) => (
                        Some(ScalarValue::TimestampNanosecond(
                            Some(ts_min_max.min),
                            TIME_DATA_TIMEZONE(),
                        )),
                        Some(ScalarValue::TimestampNanosecond(
                            Some(ts_min_max.max),
                            TIME_DATA_TIMEZONE(),
                        )),
                    ),
                    None => {
                        let range =
                            ranges.and_then(|ranges| ranges.get::<str>(field.name().as_ref()));
                        (
                            range.map(|r| r.min_value.as_ref().clone()),
                            range.map(|r| r.max_value.as_ref().clone()),
                        )
                    }
                };

                ColumnStatistics {
                    null_count: Some(0),
                    max_value,
                    min_value,
                    distinct_count: None,
                }
            }
            _ => ranges
                .and_then(|ranges| ranges.get::<str>(field.name().as_ref()))
                .map(|range| ColumnStatistics {
                    null_count: None,
                    max_value: Some(range.max_value.as_ref().clone()),
                    min_value: Some(range.min_value.as_ref().clone()),
                    distinct_count: None,
                })
                .unwrap_or_default(),
        };
        columns.push(stats)
    }

    Statistics {
        num_rows: row_count,
        total_byte_size: None,
        column_statistics: Some(columns),
        is_exact: true,
    }
}

#[cfg(test)]
mod tests {
    use schema::{InfluxFieldType, SchemaBuilder, TIME_COLUMN_NAME};

    use super::*;

    #[test]
    fn test_create_chunk_statistics_no_columns_no_rows() {
        let schema = SchemaBuilder::new().build().unwrap();
        let row_count = 0;

        let actual = create_chunk_statistics(Some(row_count), &schema, None, None);
        let expected = Statistics {
            num_rows: Some(row_count),
            total_byte_size: None,
            column_statistics: Some(vec![]),
            is_exact: true,
        };
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_create_chunk_statistics_no_columns_null_rows() {
        let schema = SchemaBuilder::new().build().unwrap();

        let actual = create_chunk_statistics(None, &schema, None, None);
        let expected = Statistics {
            num_rows: None,
            total_byte_size: None,
            column_statistics: Some(vec![]),
            is_exact: true,
        };
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_create_chunk_statistics() {
        let schema = full_schema();
        let ts_min_max = TimestampMinMax { min: 10, max: 20 };
        let ranges = Arc::new(HashMap::from([
            (
                Arc::from("tag1"),
                ColumnRange {
                    min_value: Arc::new(ScalarValue::from("aaa")),
                    max_value: Arc::new(ScalarValue::from("bbb")),
                },
            ),
            (
                Arc::from("tag3"), // does not exist in schema
                ColumnRange {
                    min_value: Arc::new(ScalarValue::from("ccc")),
                    max_value: Arc::new(ScalarValue::from("ddd")),
                },
            ),
            (
                Arc::from("field_integer"),
                ColumnRange {
                    min_value: Arc::new(ScalarValue::from(10i64)),
                    max_value: Arc::new(ScalarValue::from(20i64)),
                },
            ),
        ]));

        for row_count in [0usize, 1337usize] {
            let actual =
                create_chunk_statistics(Some(row_count), &schema, Some(ts_min_max), Some(&ranges));
            let expected = Statistics {
                num_rows: Some(row_count),
                total_byte_size: None,
                column_statistics: Some(vec![
                    ColumnStatistics {
                        null_count: None,
                        min_value: Some(ScalarValue::from("aaa")),
                        max_value: Some(ScalarValue::from("bbb")),
                        distinct_count: None,
                    },
                    ColumnStatistics::default(),
                    ColumnStatistics::default(),
                    ColumnStatistics::default(),
                    ColumnStatistics {
                        null_count: None,
                        min_value: Some(ScalarValue::from(10i64)),
                        max_value: Some(ScalarValue::from(20i64)),
                        distinct_count: None,
                    },
                    ColumnStatistics::default(),
                    ColumnStatistics::default(),
                    ColumnStatistics {
                        null_count: Some(0),
                        min_value: Some(ScalarValue::TimestampNanosecond(
                            Some(10),
                            TIME_DATA_TIMEZONE(),
                        )),
                        max_value: Some(ScalarValue::TimestampNanosecond(
                            Some(20),
                            TIME_DATA_TIMEZONE(),
                        )),
                        distinct_count: None,
                    },
                ]),
                is_exact: true,
            };
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn test_create_chunk_statistics_ts_min_max_overrides_column_range() {
        let schema = full_schema();
        let row_count = 42usize;
        let ts_min_max = TimestampMinMax { min: 10, max: 20 };
        let ranges = Arc::new(HashMap::from([(
            Arc::from(TIME_COLUMN_NAME),
            ColumnRange {
                min_value: Arc::new(ScalarValue::TimestampNanosecond(
                    Some(12),
                    TIME_DATA_TIMEZONE(),
                )),
                max_value: Arc::new(ScalarValue::TimestampNanosecond(
                    Some(22),
                    TIME_DATA_TIMEZONE(),
                )),
            },
        )]));

        let actual =
            create_chunk_statistics(Some(row_count), &schema, Some(ts_min_max), Some(&ranges));
        let expected = Statistics {
            num_rows: Some(row_count),
            total_byte_size: None,
            column_statistics: Some(vec![
                ColumnStatistics::default(),
                ColumnStatistics::default(),
                ColumnStatistics::default(),
                ColumnStatistics::default(),
                ColumnStatistics::default(),
                ColumnStatistics::default(),
                ColumnStatistics::default(),
                ColumnStatistics {
                    null_count: Some(0),
                    min_value: Some(ScalarValue::TimestampNanosecond(
                        Some(10),
                        TIME_DATA_TIMEZONE(),
                    )),
                    max_value: Some(ScalarValue::TimestampNanosecond(
                        Some(20),
                        TIME_DATA_TIMEZONE(),
                    )),
                    distinct_count: None,
                },
            ]),
            is_exact: true,
        };
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_create_chunk_statistics_ts_min_max_none_so_fallback_to_column_range() {
        let schema = full_schema();
        let row_count = 42usize;
        let ranges = Arc::new(HashMap::from([(
            Arc::from(TIME_COLUMN_NAME),
            ColumnRange {
                min_value: Arc::new(ScalarValue::TimestampNanosecond(
                    Some(12),
                    TIME_DATA_TIMEZONE(),
                )),
                max_value: Arc::new(ScalarValue::TimestampNanosecond(
                    Some(22),
                    TIME_DATA_TIMEZONE(),
                )),
            },
        )]));

        let actual = create_chunk_statistics(Some(row_count), &schema, None, Some(&ranges));
        let expected = Statistics {
            num_rows: Some(row_count),
            total_byte_size: None,
            column_statistics: Some(vec![
                ColumnStatistics::default(),
                ColumnStatistics::default(),
                ColumnStatistics::default(),
                ColumnStatistics::default(),
                ColumnStatistics::default(),
                ColumnStatistics::default(),
                ColumnStatistics::default(),
                ColumnStatistics {
                    null_count: Some(0),
                    min_value: Some(ScalarValue::TimestampNanosecond(
                        Some(12),
                        TIME_DATA_TIMEZONE(),
                    )),
                    max_value: Some(ScalarValue::TimestampNanosecond(
                        Some(22),
                        TIME_DATA_TIMEZONE(),
                    )),
                    distinct_count: None,
                },
            ]),
            is_exact: true,
        };
        assert_eq!(actual, expected);
    }

    fn full_schema() -> Schema {
        SchemaBuilder::new()
            .tag("tag1")
            .tag("tag2")
            .influx_field("field_bool", InfluxFieldType::Boolean)
            .influx_field("field_float", InfluxFieldType::Float)
            .influx_field("field_integer", InfluxFieldType::Integer)
            .influx_field("field_string", InfluxFieldType::String)
            .influx_field("field_uinteger", InfluxFieldType::UInteger)
            .timestamp()
            .build()
            .unwrap()
    }
}
