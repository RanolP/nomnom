//! The biggest individual files anywhere in the tree.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use super::{Catalog, NodeId};
use crate::scan::EntryKind;

/// The `n` largest files by logical size, biggest first, ties by lower id.
///
/// A min-heap capped at `n` keeps memory at O(n) rather than sorting every
/// file of a multi-million-node volume.
pub fn largest_files(catalog: &Catalog, n: usize) -> Vec<NodeId> {
    if n == 0 {
        return Vec::new();
    }
    // Ordered so the heap's top is the entry to evict: smallest size, and among
    // equal sizes the highest id.
    let mut heap: BinaryHeap<Reverse<(u64, Reverse<NodeId>)>> = BinaryHeap::with_capacity(n + 1);
    for node in catalog.nodes().filter(|node| node.kind == EntryKind::File) {
        let key = Reverse((node.size, Reverse(node.id)));
        if heap.len() < n {
            heap.push(key);
        } else if heap.peek().is_some_and(|top| key < *top) {
            heap.pop();
            heap.push(key);
        }
    }
    // Ascending in `Reverse` order is descending by size.
    heap.into_sorted_vec().into_iter().map(|Reverse((_, Reverse(id)))| id).collect()
}
