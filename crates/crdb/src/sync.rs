use crate::{
    btree::{self, Bias},
    digest::{Digest, DigestSequence},
    messages::{Operation, PublishOperations},
    OperationId,
};
use std::{
    cmp::{self, Ordering},
    iter,
    ops::{Range, RangeBounds},
};

struct SyncRequest {
    digests: Vec<Digest>,
}

struct SyncResponse {
    shared_prefix_end: usize,
    operations: Vec<Operation>,
}

struct SyncStats {
    roundtrips: usize,
    server_operations: usize,
    client_operations: usize,
}

fn sync_server(
    operations: &mut btree::Sequence<Operation>,
    sync_request: SyncRequest,
) -> SyncResponse {
    for client_digest in sync_request.digests {
        let server_digest = digest_for_range(operations, 0..client_digest.count);
        if server_digest == client_digest {
            return SyncResponse {
                shared_prefix_end: server_digest.count,
                operations: operations_for_range(operations, server_digest.count..)
                    .cloned()
                    .collect(),
            };
        }
    }

    SyncResponse {
        shared_prefix_end: 0,
        operations: operations.iter().cloned().collect(),
    }
}

fn publish_operations(
    server_operations: &mut btree::Sequence<Operation>,
    request: PublishOperations,
) {
    server_operations.edit(
        request
            .operations
            .into_iter()
            .map(btree::Edit::Insert)
            .collect(),
        &(),
    );
}

fn sync_client(
    client_operations: &mut btree::Sequence<Operation>,
    server_operations: &mut btree::Sequence<Operation>,
    min_digest_delta: usize,
    max_digest_count: usize,
) -> SyncStats {
    let mut client_operation_count = client_operations.summary().digest.count;
    let mut digests = Vec::new();
    let mut n = client_operation_count;

    // We will multiply by some some factor less than 1 to produce digests
    // over ever smaller digest ranges.
    // op_count * factor^max_digest_count = min_digest_size
    // factor^max_digest_count = min_digest_size/op_count
    // max_digest_count * log(factor) = log(min_digest_size/op_count)
    // log(factor) = log(min_digest_size/op_count)/max_digest_count
    // factor = base^(log(min_digest_size/op_count)/max_digest_count)
    let factor = 2f64.powf(
        (min_digest_delta as f64 / client_operation_count as f64).log2() / max_digest_count as f64,
    );
    for _ in 0..max_digest_count {
        if n <= min_digest_delta {
            break;
        }

        digests.push(digest_for_range(client_operations, 0..n));
        n = (n as f64 * factor).ceil() as usize; // 🪬
    }

    let response = sync_server(server_operations, SyncRequest { digests });
    let client_suffix = operations_for_range(client_operations, response.shared_prefix_end..)
        .cloned()
        .collect::<Vec<_>>();
    let sync_stats = SyncStats {
        roundtrips: 1,
        server_operations: response.operations.len(),
        client_operations: client_suffix.len(),
    };
    client_operations.edit(
        response
            .operations
            .into_iter()
            .map(btree::Edit::Insert)
            .collect(),
        &(),
    );
    publish_operations(
        server_operations,
        PublishOperations {
            repo_id: Default::default(),
            operations: client_suffix,
        },
    );

    sync_stats
}

impl btree::Item for Operation {
    type Summary = OperationSummary;

    fn summary(&self) -> Self::Summary {
        OperationSummary {
            digest: Digest::from(self),
        }
    }
}

impl btree::KeyedItem for Operation {
    type Key = OperationId;

    fn key(&self) -> Self::Key {
        self.id()
    }
}

#[derive(Clone, Debug, Default)]
pub struct OperationSummary {
    digest: Digest,
}

impl btree::Summary for OperationSummary {
    type Context = ();

    fn add_summary(&mut self, summary: &Self, _: &()) {
        Digest::add_summary(&mut self.digest, &summary.digest, &());
    }
}

impl btree::Dimension<'_, OperationSummary> for OperationId {
    fn add_summary(&mut self, summary: &'_ OperationSummary, _: &()) {
        *self = summary.digest.max_op_id;
    }
}

impl btree::Dimension<'_, OperationSummary> for usize {
    fn add_summary(&mut self, summary: &'_ OperationSummary, _: &()) {
        *self += summary.digest.count;
    }
}

impl btree::Dimension<'_, OperationSummary> for Digest {
    fn add_summary(&mut self, summary: &'_ OperationSummary, _: &()) {
        Digest::add_summary(self, &summary.digest, &());
    }
}

fn request_digests(
    operations: &btree::Sequence<Operation>,
    mut root_range: Range<usize>,
    count: usize,
    min_operations: usize,
) -> Vec<Digest> {
    root_range.start = cmp::min(root_range.start, operations.summary().digest.count);
    root_range.end = cmp::min(root_range.end, operations.summary().digest.count);
    subdivide_range(root_range, count, min_operations)
        .map(|range| digest_for_range(operations, range))
        .collect()
}

fn subdivide_range(
    root_range: Range<usize>,
    count: usize,
    min_operations: usize,
) -> impl Iterator<Item = Range<usize>> {
    let subrange_len = cmp::max(min_operations, (root_range.len() + count - 1) / count);

    let mut subrange_start = root_range.start;
    iter::from_fn(move || {
        if subrange_start >= root_range.end {
            return None;
        }
        let subrange = subrange_start..cmp::min(subrange_start + subrange_len, root_range.end);
        subrange_start = subrange.end;
        Some(subrange)
    })
}

fn sync(
    client: &mut btree::Sequence<Operation>,
    server: &mut btree::Sequence<Operation>,
    max_digests: usize,
    min_operations: usize,
) -> SyncStats {
    let mut server_digests = DigestSequence::new();
    let mut stats = SyncStats {
        roundtrips: 1,
        server_operations: 0,
        client_operations: 0,
    };
    let digests = request_digests(server, 0..usize::MAX, max_digests, min_operations);
    server_digests.splice(0..0, digests.iter().cloned());
    let server_operation_count = server_digests.operation_count();
    let max_sync_range = 0..(client.summary().digest.count + server_operation_count);
    let mut stack =
        subdivide_range(max_sync_range, max_digests, min_operations).collect::<Vec<_>>();
    stack.reverse();

    let mut missed_server_ops = Vec::new();
    let mut server_end = 0;
    let mut synced_end = 0;
    while let Some(mut sync_range) = stack.pop() {
        sync_range.start = cmp::max(sync_range.start, synced_end);
        if sync_range.start >= client.summary().digest.count || server_end >= server_operation_count
        {
            // We've exhausted all operations from either the client or the server, so we
            // can fast track to publishing anything the server hasn't seen and requesting
            // anything the client hasn't seen.
            break;
        } else if sync_range.end < synced_end {
            // This range has already been synced, so we can skip it.
            continue;
        }

        let (op_range, server_digest) = server_digests.digest(sync_range.clone());
        sync_range.end = cmp::max(sync_range.start + server_digest.count, sync_range.end);
        let mut server_range = server_end..server_end + sync_range.len();

        let client_digest = digest_for_range(client, sync_range.clone());
        if client_digest == server_digest {
            log::debug!("skipping {:?}", sync_range);
            synced_end = sync_range.end;
            server_end += server_digest.count;
        } else {
            let client_start_op = operations_for_range(client, sync_range.start..)
                .next()
                .map(|op| op.id())
                .unwrap();
            let client_end_op = operations_for_range(client, sync_range.start + 1..)
                .next()
                .map(|op| op.id())
                .unwrap();
            let recurse = client_start_op < op_range.end && client_end_op > op_range.start;
            while let Some(next_sync_range) = stack.last_mut() {
                let max_end = cmp::max(sync_range.end, next_sync_range.end);
                let mut merged_sync_range = sync_range.start..max_end;
                let (merged_op_range, merged_digest) =
                    server_digests.digest(merged_sync_range.clone());
                merged_sync_range.end = cmp::max(
                    merged_sync_range.start + merged_digest.count,
                    merged_sync_range.end,
                );
                let intersects =
                    client_start_op < merged_op_range.end && client_end_op > merged_op_range.start;
                if intersects {
                    break;
                } else {
                    sync_range.end = merged_sync_range.end;
                    server_range.end = server_end + sync_range.len();
                    stack.pop();
                }
            }

            if sync_range.len() > min_operations && recurse {
                log::debug!("descending into {:?}", sync_range);
                stats.roundtrips += 1;
                let digests =
                    request_digests(server, server_range.clone(), max_digests, min_operations);
                server_digests.splice(sync_range.clone(), digests.iter().cloned());
                let old_stack_len = stack.len();

                stack.extend(subdivide_range(sync_range, max_digests, min_operations));
                stack[old_stack_len..].reverse();
            } else {
                log::debug!(
                    "fetching operations for {:?} (server range: {:?})",
                    sync_range,
                    server_range,
                );
                stats.roundtrips += 1;
                let server_operations = request_operations(server, server_range.clone());
                debug_assert!(server_operations.len() > 0);
                server_digests.splice(
                    sync_range.clone(),
                    server_operations.iter().map(|op| op.into()),
                );

                let mut missed_client_ops = Vec::new();
                stats.server_operations += server_operations.len();
                let mut server_operations = server_operations.into_iter().peekable();
                let mut client_operations =
                    operations_for_range(&client, sync_range.clone()).peekable();
                for _ in sync_range.clone() {
                    match (client_operations.peek(), server_operations.peek()) {
                        (Some(client_operation), Some(server_operation)) => {
                            match client_operation.id().cmp(&server_operation.id()) {
                                Ordering::Less => {
                                    let client_operation = client_operations.next().unwrap();
                                    missed_server_ops
                                        .push(btree::Edit::Insert(client_operation.clone()));
                                    server_digests
                                        .splice(synced_end..synced_end, [client_operation.into()]);
                                }
                                Ordering::Equal => {
                                    client_operations.next().unwrap();
                                    server_operations.next().unwrap();
                                    server_end += 1;
                                }
                                Ordering::Greater => {
                                    let server_operation = server_operations.next().unwrap();
                                    missed_client_ops.push(btree::Edit::Insert(server_operation));
                                    server_end += 1;
                                }
                            }
                        }
                        (None, Some(_)) => {
                            let server_operation = server_operations.next().unwrap();
                            missed_client_ops.push(btree::Edit::Insert(server_operation));
                            server_end += 1;
                        }
                        (Some(_), None) => {
                            let client_operation = client_operations.next().unwrap();
                            missed_server_ops.push(btree::Edit::Insert(client_operation.clone()));
                            server_digests
                                .splice(synced_end..synced_end, [client_operation.into()]);
                        }
                        (None, None) => break,
                    }

                    synced_end += 1;
                }

                drop(client_operations);
                client.edit(missed_client_ops, &());
            }
        }
    }

    // Fetch and publish the remaining suffixes.
    stats.roundtrips += 1;
    if synced_end < client.summary().digest.count || server_end < server_operation_count {
        log::debug!("sending client operations from {:?}..", synced_end);
        let remaining_client_ops = operations_for_range(&client, synced_end..);
        missed_server_ops.extend(remaining_client_ops.cloned().map(btree::Edit::Insert));

        log::debug!("getting server operations from {:?}..", server_end);
        let remaining_server_ops = request_operations(server, server_end..);
        stats.server_operations += remaining_server_ops.len();
        client.edit(
            remaining_server_ops
                .into_iter()
                .map(btree::Edit::Insert)
                .collect(),
            &(),
        );
    }

    stats.client_operations = missed_server_ops.len();

    server.edit(missed_server_ops, &());
    stats
}

fn digest_for_range(operations: &btree::Sequence<Operation>, range: Range<usize>) -> Digest {
    let mut cursor = operations.cursor::<usize>();
    cursor.seek(&range.start, Bias::Right, &());
    cursor.summary(&range.end, Bias::Right, &())
}

fn request_operations<T: RangeBounds<usize>>(
    operations: &btree::Sequence<Operation>,
    range: T,
) -> Vec<Operation> {
    operations_for_range(operations, range).cloned().collect()
}

fn operations_for_range<T: RangeBounds<usize>>(
    operations: &btree::Sequence<Operation>,
    range: T,
) -> impl Iterator<Item = &Operation> {
    let mut cursor = operations.cursor::<usize>();
    match range.start_bound() {
        collections::Bound::Included(start) => {
            cursor.seek(start, Bias::Right, &());
        }
        collections::Bound::Excluded(start) => {
            cursor.seek(&(*start + 1), Bias::Right, &());
        }
        collections::Bound::Unbounded => cursor.next(&()),
    }

    iter::from_fn(move || {
        if range.contains(cursor.start()) {
            let operation = cursor.item()?;
            cursor.next(&());
            Some(operation)
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{operations, OperationCount, ReplicaId};
    use rand::prelude::*;
    use std::env;

    #[test]
    fn test_sync() {
        // assert_sync(1..=15, (1..=5).chain(7..=15));
        // assert_sync(1..=10, 5..=10);
        // assert_sync(1..=10, 4..=10);
        // assert_sync(1..=10, 1..=5);
        // assert_sync([1, 3, 5, 7, 9], [2, 4, 6, 8, 10]);
        // assert_sync([1, 2, 3, 4, 6, 7, 8, 9, 11, 12], [4, 5, 6, 10, 12]);
        // assert_sync(1..=10, 5..=14);
        // assert_sync(1..=80, (1..=70).chain(90..=100));
        // assert_sync(1..=1910, (1..=1900).chain(1910..=2000));
        assert_sync(
            (1..=1500).chain(4000..=10000),
            (1..=1000).chain(4000..=11000),
        );
    }

    #[gpui::test(iterations = 100)]
    fn test_random(mut rng: StdRng) {
        let max_operations = env::var("OPERATIONS")
            .map(|i| i.parse().expect("invalid `OPERATIONS` variable"))
            .unwrap_or(10);
        let max_digest_count = 4096;
        let min_operations = 4096;

        let mut connected = true;
        let mut client_ops = btree::Sequence::<Operation>::new();
        let mut server_ops = btree::Sequence::<Operation>::new();
        let mut ideal_server_ops = 0;
        let mut ideal_client_ops = 0;
        let mut next_reconnection = None;
        for ix in 1..=max_operations {
            if connected && rng.gen_bool(0.0005) {
                dbg!(ix);
                connected = false;

                let mut factor = 0.0005;
                while rng.gen() {
                    factor *= 2.0;
                }

                let remaining_operations = max_operations - ix;
                let disconnection_period = (remaining_operations as f64 * factor) as usize;
                next_reconnection = Some(ix + disconnection_period);
                dbg!(disconnection_period);
            }

            if next_reconnection == Some(ix) {
                connected = true;
                next_reconnection = None;
                log::debug!("===============");
                // log::debug!(
                //     "client ops: {:?}",
                //     client_ops.iter().map(|op| op.id()).collect::<Vec<_>>()
                // );
                // log::debug!(
                //     "server ops: {:?}",
                //     server_ops.iter().map(|op| op.id()).collect::<Vec<_>>()
                // );

                let stats = sync(
                    &mut client_ops,
                    &mut server_ops,
                    max_digest_count,
                    min_operations,
                );
                log::debug!("roundtrips: {}", stats.roundtrips);
                log::debug!(
                    "ideal server ops: {}, actual server ops: {}, abs error: {}, pct error: {:.3}%",
                    ideal_server_ops,
                    stats.server_operations,
                    stats.server_operations - ideal_server_ops,
                    ((stats.server_operations as f64 / ideal_server_ops as f64) - 1.) * 100.
                );
                log::debug!(
                    "ideal client ops: {}, actual client ops: {}, abs error: {}, pct error: {:.3}%",
                    ideal_client_ops,
                    stats.client_operations,
                    stats.client_operations - ideal_client_ops,
                    ((stats.client_operations as f64 / ideal_client_ops as f64) - 1.0) * 100.
                );

                assert_eq!(
                    client_ops.iter().map(|op| op.id()).collect::<Vec<_>>(),
                    server_ops.iter().map(|op| op.id()).collect::<Vec<_>>()
                );
                ideal_client_ops = 0;
                ideal_server_ops = 0;
            }

            if connected {
                let replica_id = ReplicaId(rng.gen_range(0..=1));
                client_ops.insert_or_replace(build_operation2(replica_id, ix), &());
                server_ops.insert_or_replace(build_operation2(replica_id, ix), &());
            } else if rng.gen_bool(0.95) {
                ideal_server_ops += 1;
                server_ops.insert_or_replace(build_operation2(ReplicaId(0), ix), &());
            } else {
                ideal_client_ops += 1;
                client_ops.insert_or_replace(build_operation2(ReplicaId(1), ix), &());
            }
        }

        log::debug!("============");
        // log::debug!(
        //     "client ops: {:?}",
        //     client_ops.iter().map(|op| op.id()).collect::<Vec<_>>()
        // );
        // log::debug!(
        //     "server ops: {:?}",
        //     server_ops.iter().map(|op| op.id()).collect::<Vec<_>>()
        // );
        let stats = sync(
            &mut client_ops,
            &mut server_ops,
            max_digest_count,
            min_operations,
        );
        log::debug!("roundtrips: {}", stats.roundtrips);
        log::debug!(
            "ideal server ops: {}, actual server ops: {}, abs error: {}, pct error: {:.3}%",
            ideal_server_ops,
            stats.server_operations,
            stats.server_operations - ideal_server_ops,
            ((stats.server_operations as f64 / ideal_server_ops as f64) - 1.) * 100.
        );
        log::debug!(
            "ideal client ops: {}, actual client ops: {}, abs error: {}, pct error: {:.3}%",
            ideal_client_ops,
            stats.client_operations,
            stats.client_operations - ideal_client_ops,
            ((stats.client_operations as f64 / ideal_client_ops as f64) - 1.0) * 100.
        );
        assert_eq!(
            client_ops.iter().map(|op| op.id()).collect::<Vec<_>>(),
            server_ops.iter().map(|op| op.id()).collect::<Vec<_>>()
        );
    }

    fn assert_sync(
        client_ops: impl IntoIterator<Item = usize>,
        server_ops: impl IntoIterator<Item = usize>,
    ) {
        let client_ops = client_ops
            .into_iter()
            .map(build_operation)
            .collect::<Vec<_>>();
        let server_ops = server_ops
            .into_iter()
            .map(build_operation)
            .collect::<Vec<_>>();

        for max_digests in 256..=256 {
            for min_operations in 256..=256 {
                log::debug!(
                    "max digests: {}, min operations: {}",
                    max_digests,
                    min_operations
                );
                let mut client_operations = btree::Sequence::from_iter(client_ops.clone(), &());
                let mut server_operations = btree::Sequence::from_iter(server_ops.clone(), &());
                sync(
                    &mut client_operations,
                    &mut server_operations,
                    max_digests,
                    min_operations,
                );
                assert_eq!(
                    client_operations
                        .iter()
                        .map(|op| op.id())
                        .collect::<Vec<_>>(),
                    server_operations
                        .iter()
                        .map(|op| op.id())
                        .collect::<Vec<_>>()
                );
            }
        }
    }

    fn build_operation(id: usize) -> Operation {
        Operation::CreateBranch(operations::CreateBranch {
            id: OperationId {
                replica_id: Default::default(),
                operation_count: OperationCount(id),
            },
            parent: Default::default(),
            name: "".into(),
        })
    }

    fn build_operation2(replica_id: ReplicaId, id: usize) -> Operation {
        Operation::CreateBranch(operations::CreateBranch {
            id: OperationId {
                replica_id,
                operation_count: OperationCount(id),
            },
            parent: Default::default(),
            name: "".into(),
        })
    }

    fn digest_counts(digests: &[Digest]) -> Vec<usize> {
        digests.iter().map(|d| d.count).collect()
    }
}
