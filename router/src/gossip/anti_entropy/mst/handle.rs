//! A handle to interact with a [`AntiEntropyActor`].
//!
//! [`AntiEntropyActor`]: super::actor::AntiEntropyActor

use std::ops::RangeInclusive;

use data_types::NamespaceName;
use merkle_search_tree::digest::RootHash;
use observability_deps::tracing::error;
use tokio::sync::{mpsc, oneshot};

use super::actor::{MerkleSnapshot, Op};

/// A handle to an [`AntiEntropyActor`].
///
/// [`AntiEntropyActor`]: super::actor::AntiEntropyActor
#[derive(Debug, Clone)]
pub struct AntiEntropyHandle {
    // Non-schema actor requests.
    op_tx: mpsc::Sender<Op>,

    // Schema update notifications (prioritised by actor)
    schema_tx: mpsc::Sender<NamespaceName<'static>>,
}

impl AntiEntropyHandle {
    pub(super) fn new(
        op_tx: mpsc::Sender<Op>,
        schema_tx: mpsc::Sender<NamespaceName<'static>>,
    ) -> AntiEntropyHandle {
        Self { op_tx, schema_tx }
    }

    /// Request the [`MerkleSearchTree`] observe a new update to `name`.
    ///
    /// This call is cheap - it places `name` into a queue to be processed
    /// asynchronously by a background worker. If the queue is saturated, an
    /// error is logged and the update is dropped.
    ///
    /// # Ordering
    ///
    /// Calls to this method MUST only be made when a subsequent cache lookup
    /// would yield a schema for `name`.
    ///
    /// # Starvation
    ///
    /// The [`AntiEntropyActor`] prioritises processing upsert requests over all
    /// other operations - an extreme rate of calls to this method may adversely
    /// affect the latency of other [`AntiEntropyHandle`] methods.
    ///
    /// [`MerkleSearchTree`]: merkle_search_tree::MerkleSearchTree
    /// [`AntiEntropyActor`]: super::actor::AntiEntropyActor
    pub(crate) fn observe_update(&self, name: NamespaceName<'static>) {
        // NOTE: this doesn't send the actual schema and it likely never should.
        //
        // If this method sent `(name, schema)` tuples, it would require the
        // stream of calls to contain monotonic schemas - that means that
        // `schema` must always "go forwards", with subsequent calls containing
        // a (non-strict) superset of the content of prior calls.
        //
        // This invariant can't be preserved in a multi-threaded system where
        // updates can be applied to the same cached entry concurrently:
        //
        // - T1: cache.upsert(name, schema(table_a)) -> schema(table_a)
        // - T2: cache.upsert(name, schema(table_b)) -> schema(table_a, table_b)
        // - T2: handle.upsert(name, schema(table_a, table_b))
        // - T1: handle.upsert(name, schema(table_a))
        //
        // The last call violates the monotonicity requirement - T1 sets the
        // anti-entropy state to the pre-merged value containing only table_a,
        // overwriting the correct state that reflected both tables.
        //
        // The monotonic property is provided by the underlying cache
        // implementation; namespace schemas are always merged. By providing the
        // actor the name of the updated schema, it can read the merged and most
        // up to date schema directly from the cache itself, ensuring
        // monotonicity of the schemas regardless of call order.

        if self.schema_tx.try_send(name.clone()).is_err() {
            // If enqueuing this schema update fails, the MST will become
            // out-of-sync w.r.t the content of the cache, and falsely believe
            // the update applied to the cache in this schema (if any) has not
            // been applied locally.
            //
            // If this happens, peers may start conflict resolution rounds to
            // converge this difference, eventually causing the local node to
            // perform a no-op update to the local key, and in turn causing
            // another requeue attempt here.
            //
            // This is bad for efficiency because it causes spurious syncs, but
            // does not effect correctness due to the monotonicity of updates.
            //
            // If every peer hits this same edge case, none of the MSTs will
            // contain the updated schema, and no convergence will be attempted
            // for this update until a subsequent enqueue for "name" succeeds.
            // This does affect correctness, but is exceedingly unlikely, and
            // logged for debugging purposes.
            error!(%name, "error enqueuing schema update for anti-entropy");
        }
    }

    /// Send `name` to the MST actor to observe a new schema state.
    ///
    /// This method is the blocking variant of the non-blocking
    /// [`AntiEntropyHandle::observe_update()`] that waits for `name` to be
    /// successfully enqueued (blocking if the queue is full).
    pub async fn observe_update_blocking(&self, name: NamespaceName<'static>) {
        self.schema_tx
            .send(name)
            .await
            .expect("mst actor not running");
    }

    /// Return the current content hash ([`RootHash`]) describing the set of
    /// [`NamespaceSchema`] observed so far.
    ///
    /// [`NamespaceSchema`]: data_types::NamespaceSchema
    pub async fn content_hash(&self) -> RootHash {
        let (tx, rx) = oneshot::channel();

        self.op_tx
            .send(Op::ContentHash(tx))
            .await
            .expect("anti-entropy actor has stopped");

        rx.await.expect("anti-entropy actor has stopped")
    }

    /// Obtain a [`MerkleSnapshot`] for the current Merkle Search Tree state.
    ///
    /// A [`MerkleSnapshot`] is a compact serialised representation of the MST
    /// state.
    pub(crate) async fn snapshot(&self) -> MerkleSnapshot {
        let (tx, rx) = oneshot::channel();

        self.op_tx
            .send(Op::Snapshot(tx))
            .await
            .expect("anti-entropy actor has stopped");

        rx.await.expect("anti-entropy actor has stopped")
    }

    /// Compute the difference between the local [`MerkleSearchTree`] state, and
    /// the provided [`MerkleSnapshot`], returning the inclusive key ranges that
    /// contain inconsistencies.
    ///
    /// [`MerkleSearchTree`]: merkle_search_tree::MerkleSearchTree
    pub(crate) async fn compute_diff(
        &self,
        snap: MerkleSnapshot,
    ) -> Vec<RangeInclusive<NamespaceName<'static>>> {
        let (tx, rx) = oneshot::channel();

        self.op_tx
            .send(Op::Diff(snap, tx))
            .await
            .expect("anti-entropy actor has stopped");

        rx.await.expect("anti-entropy actor has stopped")
    }

    /// Request all [`NamespaceName`] known to the MST in the given inclusive
    /// key range.
    #[allow(dead_code)]
    pub(crate) async fn get_keys_in_range(
        &self,
        range: RangeInclusive<NamespaceName<'static>>,
    ) -> Vec<NamespaceName<'static>> {
        let (tx, rx) = oneshot::channel();

        self.op_tx
            .send(Op::KeysInRange(range, tx))
            .await
            .expect("anti-entropy actor has stopped");

        rx.await.expect("anti-entropy actor has stopped")
    }
}
