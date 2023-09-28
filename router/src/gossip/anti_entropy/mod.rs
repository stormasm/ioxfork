//! Anti-entropy primitives providing eventual consistency over gossip.
//!
//! [`NamespaceCache`] anti-entropy between gossip peers is driven by the
//! following components:
//!
//! ```text
//!                        ┌───────────────┐
//!                ┌──────▶│   MST Actor   │◀──┐
//!                │       └───────────────┘   │
//!             Schema                         │
//!            Updates                    MST Hashes
//!                │                           │
//!                │                           │
//!    ┌ ─ ─ ─ ─ ─ ┼ ─ ─ ─ ─ ─                 │
//!         NamespaceCache    │                ▼
//!    │           ▼                   ┌───────────────┐
//!        ┌──────────────┐   │        │  Convergence  │
//!    │   │ MST Observer │◀───────────│     Actor     │◀────────┐
//!        └──────────────┘   │        └───────────────┘         │
//!    │                                       ▲                 │
//!     ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┘                │                 │
//!                                       Consistency        Diffs &
//!                                         Probes           Schemas
//!                                            │                 │
//!                                            ▼                 ▼
//!                                    ┌───────────────┐ ┌───────────────┐
//!                                    │    Gossip     │ │   Sync RPC    │
//!                                    └───────────────┘ └───────────────┘
//! ```
//!
//! From left to right:
//!
//!   * [`MerkleTree`]: a [`NamespaceCache`] decorator observing any changes
//!         made to the local [`NamespaceCache`], providing diffs to the local
//!         node's [`MerkleSearchTree`].
//!
//!   * [`AntiEntropyActor`]: an actor task maintaining the local node's
//!         [`MerkleSearchTree`] state to accurately reflect the
//!         [`NamespaceCache`] content.
//!
//!   * [`ConvergenceActor`]: an actor task responsible for performing
//!         consistency checks with cluster peers, and driving convergence when
//!         inconsistencies are detected.
//!
//!   * [`ConsistencyProber`]: an abstract mechanism for exchanging MST
//!         consistency proofs / root hashes. Typically using gossip messages.
//!
//!   * [`RpcWorker`]: a reconciliation worker, spawned by the
//!         [`ConvergenceActor`] to perform differential convergence between the
//!         local node and an inconsistent peer. Makes RPC calls to perform MST
//!         diffs and fetch inconsistent schemas.
//!
//! [`NamespaceCache`]: crate::namespace_cache::NamespaceCache
//! [`MerkleTree`]: mst::merkle::MerkleTree
//! [`AntiEntropyActor`]: mst::actor::AntiEntropyActor
//! [`MerkleSearchTree`]: merkle_search_tree::MerkleSearchTree
//! [`ConvergenceActor`]: sync::actor::ConvergenceActor
//! [`ConsistencyProber`]: sync::consistency_prober::ConsistencyProber
//! [`RpcWorker`]: sync::rpc_worker::RpcWorker

pub mod mst;
pub mod sync;

#[cfg(test)]
pub mod prop_gen {
    use std::{
        collections::{hash_map::DefaultHasher, BTreeMap},
        hash::Hasher,
    };

    use data_types::{
        ColumnId, ColumnSchema, ColumnType, ColumnsByName, MaxColumnsPerTable, MaxTables,
        NamespaceId, NamespaceName, NamespaceSchema, TableId, TableSchema,
    };
    use proptest::prelude::*;

    /// A set of table and column names from which arbitrary names are selected
    /// in prop tests, instead of using random values that have a low
    /// probability of overlap.
    pub const TEST_TABLE_NAME_SET: &[&str] = &[
        "bananas", "quiero", "un", "platano", "donkey", "goose", "egg", "mr_toro",
    ];

    prop_compose! {
        /// Generate a series of ColumnSchema assigned randomised IDs with a
        /// stable mapping of `id -> data type`.
        ///
        /// This generates at most 255 unique columns.
        pub fn arbitrary_column_schema_stable()(id in 0_i16..255) -> ColumnSchema {
            // Provide a stable mapping of ID to data type to avoid column type
            // conflicts by reducing the ID to the data type discriminant range
            // and using that to assign the data type.
            let col_type = ColumnType::try_from((id % 7) + 1).expect("valid discriminator range");

            ColumnSchema { id: ColumnId::new(id as _), column_type: col_type }
        }
    }

    prop_compose! {
        /// Generate an arbitrary TableSchema with up to 255 columns that
        /// contain stable `column name -> data type` and `column name -> column
        /// id` mappings.
        pub fn arbitrary_table_schema()(
            name in proptest::sample::select(TEST_TABLE_NAME_SET),
            columns in proptest::collection::hash_set(
                arbitrary_column_schema_stable(),
                (0, 255) // Set size range
            ),
        ) -> (String, TableSchema) {
            // Map the column schemas into `name -> schema`, generating a
            // column name derived from the column ID to ensure a consistent
            // mapping of name -> id, and in turn, name -> data type.
            let columns = columns.into_iter()
                .map(|v| (format!("col-{}", v.id.get()), v))
                .collect::<BTreeMap<String, ColumnSchema>>();

            let columns = ColumnsByName::from(columns);
            (name.to_string(), TableSchema {
                id: deterministic_id_for_table_name(name),
                partition_template: Default::default(),
                columns,
            })
        }
    }

    prop_compose! {
        /// Generate an arbitrary NamespaceSchema that contains tables from
        /// [`TEST_TABLE_NAME_SET`], containing up to 255 columns with stable
        /// `name -> (id, data type)` mappings.
        ///
        /// Namespace IDs are allocated from the specified strategy.
        pub fn arbitrary_namespace_schema(namespace_ids: impl Strategy<Value = i64>)(
            namespace_id in namespace_ids,
            tables in proptest::collection::vec(
                arbitrary_table_schema(),
                (0, 10) // Set size range
            ),
            max_tables in 1..std::i32::MAX as usize,
            max_columns_per_table in 1..std::i32::MAX as usize,
            retention_period_ns in any::<Option<i64>>(),
        ) -> NamespaceSchema {
            let tables = tables.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
            NamespaceSchema {
                id: NamespaceId::new(namespace_id),
                tables,
                max_tables: MaxTables::try_from(max_tables).unwrap(),
                max_columns_per_table: MaxColumnsPerTable::try_from(max_columns_per_table).unwrap(),
                retention_period_ns,
                partition_template: Default::default(),
            }
        }
    }

    pub fn deterministic_name_for_schema(schema: &NamespaceSchema) -> NamespaceName<'static> {
        NamespaceName::try_from(format!("ns-{}", schema.id)).unwrap()
    }

    pub fn deterministic_id_for_table_name(name: &str) -> TableId {
        let mut h = DefaultHasher::new();
        h.write(name.as_bytes());
        TableId::new(h.finish() as _)
    }
}
