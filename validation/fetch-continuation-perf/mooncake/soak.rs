// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Randomized differential soak of [`LowerTierIndexer`] against a brute-force model.
//!
//! Each case applies random Stored, Removed, Cleared and RemoveWorker events from a few worker and
//! cache owners over short branching prompts, split across event lanes that share one edge map as
//! `ThreadPoolIndexer` lanes do. After every event the edge map and the lanes' reverse maps must
//! agree with each other and with the model; every few events, random queries must match a
//! per-worker walk of the model. `LOWER_TIER_SOAK_CASES` (default 64) and `LOWER_TIER_SOAK_SEED`
//! scale and reproduce a run; a failure names the seed of its case. Every request prefix has
//! one deterministic external hash across workers, tiers and cache owners.

use std::collections::{BTreeMap, HashMap, HashSet};

use rustc_hash::FxHashMap;

use super::{
    EdgeOwnersEntry, IndexedResidencyOwner, LowerTierContinuation, LowerTierIndexer, TransitionKey,
    WorkerBlockIndex,
};
use crate::identity::{
    CacheOwnerId, CacheSemanticsId, ClusterId, IdentitySource, IndexerDomainId, PoolId,
    RoutingScopeId, StableDpSlotId,
};
use crate::indexer::{
    LowerTierIndexers, LowerTierQueryOptions, MatchDetails, query_lower_tiers_with_options,
};
use crate::kv_hints::{KvTransferCandidateSource, KvTransferCandidates};
use crate::protocols::{
    ExternalSequenceBlockHash, KvCacheEvent, KvCacheEventData, KvCacheRemoveData, KvCacheStoreData,
    KvCacheStoredBlockData, LocalBlockHash, OverlapScores, ResidencyDomain, ResidencyOwner,
    ResidencyProjection, ResidencyRoutingSnapshot, RouterEvent, RouterHintSourceMetadata,
    StorageTier, WorkerWithDpRank,
};

const WORKER_IDS: [u64; 3] = [1, 2, 3];
const CACHE_OWNER_SLOTS: [u8; 3] = [10, 11, 12];
const OPS_PER_CASE: usize = 60;
const QUERY_EVERY: usize = 5;
const QUERIES_PER_CHECK: usize = 3;

/// Owner -> block hash -> the transition it was stored under.
type Memberships = HashMap<IndexedResidencyOwner, BTreeMap<u64, TransitionKey>>;
/// A walk's stop position, last matched hash, and each matched (position, hash).
type Walk = (
    usize,
    Option<ExternalSequenceBlockHash>,
    Vec<(usize, ExternalSequenceBlockHash)>,
);

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        mix(self.0)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

fn mix(mut x: u64) -> u64 {
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// Deterministic sequence hash shared by every owner and tier for a request prefix.
fn sequence_hash(parent: Option<u64>, local: u64) -> u64 {
    let hash = mix(parent.unwrap_or(0x5EED).wrapping_mul(31) ^ mix(local));
    // u64::MAX is the index's root-parent sentinel.
    if hash == u64::MAX { hash - 1 } else { hash }
}

struct Prompt {
    locals: Vec<u64>,
    hashes: Vec<u64>,
}

/// Short prompts over a three-symbol alphabet, half of them extending another's prefix, so
/// owners share and fork chains.
fn prompts(rng: &mut Rng) -> Vec<Prompt> {
    let mut prompts: Vec<Prompt> = Vec::new();
    for _ in 0..6 {
        let mut locals: Vec<u64> = match prompts.len() {
            0 => Vec::new(),
            n if rng.chance(50) => {
                let base = &prompts[rng.below(n)].locals;
                base[..1 + rng.below(base.len())].to_vec()
            }
            _ => Vec::new(),
        };
        let len = (1 + rng.below(8)).max(locals.len());
        while locals.len() < len {
            locals.push(1 + rng.below(3) as u64);
        }
        let mut hashes = Vec::with_capacity(len);
        for (pos, local) in locals.iter().enumerate() {
            let parent = pos.checked_sub(1).map(|prev| hashes[prev]);
            hashes.push(sequence_hash(parent, *local));
        }
        prompts.push(Prompt { locals, hashes });
    }
    prompts
}

fn cache_owner(slot: u8) -> CacheOwnerId {
    CacheOwnerId::new(
        PoolId::new(
            IndexerDomainId::new(
                CacheSemanticsId::new([1; 16], IdentitySource::Explicit),
                RoutingScopeId::new([2; 16], IdentitySource::Explicit),
            ),
            ClusterId::new(3),
        ),
        StableDpSlotId::new([slot; 16], IdentitySource::Explicit),
    )
}

fn workers() -> Vec<WorkerWithDpRank> {
    WORKER_IDS
        .iter()
        .flat_map(|&id| [WorkerWithDpRank::new(id, 0), WorkerWithDpRank::new(id, 1)])
        .collect()
}

#[derive(Clone, Copy, Debug)]
enum SoakOwner {
    Worker(WorkerWithDpRank),
    Cache(CacheOwnerId),
}

impl SoakOwner {
    fn indexed(self) -> IndexedResidencyOwner {
        IndexedResidencyOwner::from_exact(match self {
            Self::Worker(worker) => ResidencyOwner::worker(worker),
            Self::Cache(owner) => ResidencyOwner::cache_owner(owner),
        })
    }

    fn event(self, event_id: u64, data: KvCacheEventData) -> RouterEvent {
        match self {
            Self::Worker(worker) => RouterEvent::with_residency_domain(
                worker.worker_id,
                KvCacheEvent {
                    event_id,
                    data,
                    dp_rank: worker.dp_rank,
                },
                StorageTier::HostPinned,
                ResidencyDomain::Worker,
            ),
            Self::Cache(owner) => RouterEvent::with_cache_owner(
                9,
                KvCacheEvent {
                    event_id,
                    data,
                    dp_rank: 0,
                },
                StorageTier::HostPinned,
                owner,
            ),
        }
    }
}

/// The lane an owner's events land on: fixed per owner, as in `ThreadPoolIndexer`.
fn lane_of(owner: IndexedResidencyOwner, lanes: usize) -> usize {
    match owner {
        IndexedResidencyOwner::Worker(worker) => worker.worker_id as usize % lanes,
        IndexedResidencyOwner::Exact(key) => key.digest()[0] as usize % lanes,
    }
}

/// The index's documented semantics, without its data structures.
#[derive(Default)]
struct Model {
    owners: Memberships,
}

impl Model {
    fn edge_child(&self, key: TransitionKey) -> Option<u64> {
        self.owners
            .values()
            .flat_map(|blocks| blocks.iter())
            .find_map(|(hash, held)| (*held == key).then_some(*hash))
    }

    fn holders(&self, key: TransitionKey) -> impl Iterator<Item = IndexedResidencyOwner> + '_ {
        self.owners
            .iter()
            .filter(move |(_, blocks)| blocks.values().any(|held| *held == key))
            .map(|(owner, _)| *owner)
    }

    /// Stops at a block the owner holds under another transition, or whose transition already
    /// leads to a different child: the existing mapping wins.
    fn store(&mut self, owner: IndexedResidencyOwner, parent: Option<u64>, blocks: &[(u64, u64)]) {
        let mut parent = parent;
        for &(local, hash) in blocks {
            let key =
                TransitionKey::new(parent.map(ExternalSequenceBlockHash), LocalBlockHash(local));
            if self
                .owners
                .get(&owner)
                .and_then(|held| held.get(&hash))
                .is_some_and(|held| *held != key)
            {
                break;
            }
            if self.edge_child(key).is_some_and(|child| child != hash) {
                break;
            }
            self.owners.entry(owner).or_default().insert(hash, key);
            parent = Some(hash);
        }
    }

    fn remove(&mut self, owner: IndexedResidencyOwner, hashes: &[u64]) {
        let Some(blocks) = self.owners.get_mut(&owner) else {
            return;
        };
        for hash in hashes {
            // Children remain indexed even when their parent membership disappears.
            blocks.remove(hash);
        }
        if blocks.is_empty() {
            self.owners.remove(&owner);
        }
    }

    fn remove_worker(&mut self, worker_id: u64) {
        self.owners.retain(|owner, _| {
            !matches!(owner, IndexedResidencyOwner::Worker(worker) if worker.worker_id == worker_id)
        });
    }

    /// Walks from `continuation` while some holder of each transition satisfies `holds`.
    fn walk(
        &self,
        local: &[LocalBlockHash],
        continuation: LowerTierContinuation,
        holds: impl Fn(IndexedResidencyOwner) -> bool,
    ) -> Walk {
        let (mut pos, mut cur) = (continuation.start_pos, continuation.last_matched_hash);
        let mut matched = Vec::new();
        while pos < local.len() {
            let key = TransitionKey::new(cur, local[pos]);
            let Some(child) = self.edge_child(key) else {
                break;
            };
            if !self.holders(key).any(&holds) {
                break;
            }
            cur = Some(ExternalSequenceBlockHash(child));
            matched.push((pos, ExternalSequenceBlockHash(child)));
            pos += 1;
        }
        (pos, cur, matched)
    }
}

/// The lanes' reverse maps, merged; an owner must live on one lane only.
fn lane_memberships(lanes: &[WorkerBlockIndex], ctx: &str) -> Memberships {
    let mut merged = Memberships::new();
    for lane in lanes {
        for (owner, state) in lane {
            assert_eq!(
                IndexedResidencyOwner::from_exact(state.owner),
                *owner,
                "{ctx}: reverse-map key and stored owner differ"
            );
            assert!(
                !state.blocks.is_empty(),
                "{ctx}: empty reverse-map owner entry for {owner:?}"
            );
            let blocks = state
                .blocks
                .iter()
                .map(|(hash, key)| (hash.0, *key))
                .collect();
            assert!(
                merged.insert(*owner, blocks).is_none(),
                "{ctx}: {owner:?} is indexed on two lanes"
            );
        }
    }
    merged
}

/// Every edge membership is in its owner's reverse map under that edge's key and child, and the
/// counts match, so the two views are the same set. Also checks the owner-set representation.
fn check_edges(index: &LowerTierIndexer, members: &Memberships, ctx: &str) {
    let mut edge_memberships = 0;
    for edge in index.edges.iter() {
        let key = *edge.key();
        let child = edge.value().child_hash().0;
        let owners: Vec<IndexedResidencyOwner> = match edge.value() {
            EdgeOwnersEntry::Single { owner, .. } => vec![index.owners.owner(*owner)],
            EdgeOwnersEntry::Pair(pair) => {
                assert_ne!(
                    pair.owners[0], pair.owners[1],
                    "{ctx}: pair repeats an owner at {key:?}"
                );
                pair.owners
                    .iter()
                    .map(|id| index.owners.owner(*id))
                    .collect()
            }
            EdgeOwnersEntry::Multi(multi) => {
                assert!(
                    multi.len() > 2,
                    "{ctx}: multi edge with {} owners at {key:?}",
                    multi.len()
                );
                let owners: Vec<_> = multi.owners().collect();
                let fingerprint = owners.iter().fold(0, |fp, owner| fp ^ owner.fingerprint());
                assert_eq!(
                    fingerprint, multi.owners_fp,
                    "{ctx}: stale fingerprint at {key:?}"
                );
                owners
            }
        };
        for owner in owners {
            edge_memberships += 1;
            assert_eq!(
                members.get(&owner).and_then(|blocks| blocks.get(&child)),
                Some(&key),
                "{ctx}: edge {key:?} -> {child} lists {owner:?}, whose reverse map disagrees"
            );
        }
    }
    let reverse_memberships: usize = members.values().map(BTreeMap::len).sum();
    assert_eq!(
        edge_memberships, reverse_memberships,
        "{ctx}: edge and reverse-map membership counts differ"
    );
}

fn check_queries(
    rng: &mut Rng,
    index: &LowerTierIndexer,
    model: &Model,
    prompts: &[Prompt],
    cache_owners: &[CacheOwnerId],
    ctx: &str,
) {
    for query in 0..QUERIES_PER_CHECK {
        let ctx = format!("{ctx} query={query}");
        let prompt = &prompts[rng.below(prompts.len())];
        let mut local: Vec<LocalBlockHash> = prompt.locals[..1 + rng.below(prompt.locals.len())]
            .iter()
            .map(|value| LocalBlockHash(*value))
            .collect();
        if rng.chance(30) {
            local.push(LocalBlockHash(1 + rng.below(3) as u64));
        }

        let all_workers = workers();
        let mut attachments = Vec::new();
        for owner in cache_owners {
            if rng.chance(75) {
                attachments.push((*owner, all_workers[rng.below(all_workers.len())]));
            }
        }
        let projection = ResidencyProjection::new(attachments.iter().copied()).unwrap();
        let hint_sources: Vec<CacheOwnerId> = cache_owners
            .iter()
            .copied()
            .filter(|_| rng.chance(60))
            .collect();
        let snapshot = ResidencyRoutingSnapshot::new(
            projection.clone(),
            hint_sources.iter().map(|owner| {
                (
                    *owner,
                    RouterHintSourceMetadata {
                        source_control_endpoint: "tcp://source:9000".to_string(),
                        worker_type: "decode".to_string(),
                        topology_domains: Default::default(),
                    },
                    projection.cache_owner_worker(*owner),
                )
            }),
        );

        // A device prefix may exist even when its host edges have been removed.
        let mut root_chain = Vec::with_capacity(local.len());
        for &LocalBlockHash(local) in &local {
            let parent = root_chain
                .last()
                .map(|hash: &ExternalSequenceBlockHash| hash.0);
            root_chain.push(ExternalSequenceBlockHash(sequence_hash(parent, local)));
        }
        let mut continuations = FxHashMap::default();
        for worker in &all_workers {
            if !rng.chance(60) {
                continue;
            }
            let continuation = match rng.below(root_chain.len() + 1) {
                0 => LowerTierContinuation::from_root(0),
                start => LowerTierContinuation::new(start, root_chain[start - 1]),
            };
            continuations.insert(*worker, continuation);
        }

        // As the router does, the lower tier's root workers join at position 0.
        let root_key = TransitionKey::new(None, local[0]);
        let root_workers = index.root_workers(local[0], &projection);
        let expected_root: HashSet<_> = model
            .holders(root_key)
            .filter_map(|owner| owner.project(&projection))
            .collect();
        assert_eq!(
            root_workers.len(),
            expected_root.len(),
            "{ctx}: root workers repeat"
        );
        assert_eq!(
            root_workers.iter().copied().collect::<HashSet<_>>(),
            expected_root,
            "{ctx}: root workers"
        );
        for worker in root_workers {
            continuations
                .entry(worker)
                .or_insert(LowerTierContinuation::from_root(0));
        }

        let retain = rng.chance(70);
        let details = index.query_match_details_with_options_and_snapshot(
            &local,
            &continuations,
            retain,
            &snapshot,
        );
        assert_eq!(
            details.hits.len(),
            continuations.len(),
            "{ctx}: hits cover every worker"
        );

        let recorded: HashMap<usize, ExternalSequenceBlockHash> = details
            .kv_transfer_extensions
            .iter()
            .flat_map(|extensions| extensions.block_hashes.iter().copied())
            .collect();
        let credited = |source| {
            details
                .kv_transfer_extensions
                .as_ref()
                .and_then(|extensions| extensions.owner_prefix_blocks.get(&source).copied())
        };
        let assert_on_chain =
            |blocks: &[(usize, ExternalSequenceBlockHash)], below: usize, who: &str| {
                for (pos, hash) in blocks.iter().filter(|(pos, _)| *pos < below) {
                    assert_eq!(
                        recorded.get(pos),
                        Some(hash),
                        "{ctx}: {who} credited past a foreign hash at {pos}"
                    );
                }
            };

        for (worker, continuation) in &continuations {
            let (end, last, matched) = model.walk(&local, *continuation, |owner| {
                owner.project(&projection) == Some(*worker)
            });
            let hits = end - continuation.start_pos;
            assert_eq!(
                details.hits.get(worker),
                Some(&hits),
                "{ctx}: hits for {worker:?} from {continuation:?} over {local:?}"
            );
            let next = match hits {
                0 => *continuation,
                _ => LowerTierContinuation {
                    start_pos: end,
                    last_matched_hash: last,
                },
            };
            assert_eq!(
                details.next_continuations.get(worker),
                Some(&next),
                "{ctx}: next for {worker:?}"
            );
            if !retain {
                continue;
            }
            let prefix = credited(KvTransferCandidateSource::Worker(*worker));
            assert_eq!(
                prefix.is_some(),
                hits > 0,
                "{ctx}: {worker:?} credited iff it matched"
            );
            let Some(prefix) = prefix else {
                continue;
            };
            assert_eq!(
                prefix, end,
                "{ctx}: {worker:?} prefix differs from its walk end"
            );
            assert_on_chain(&matched, prefix, &format!("{worker:?}"));
        }

        if !retain {
            assert!(
                details.kv_transfer_extensions.is_none(),
                "{ctx}: extensions without retain"
            );
            continue;
        }
        let extensions = details.kv_transfer_extensions.as_ref().expect("retained");
        assert!(
            extensions
                .block_hashes
                .windows(2)
                .all(|pair| pair[0].0 < pair[1].0),
            "{ctx}: recorded chain positions are not strictly increasing"
        );
        let hint_keys: HashSet<_> = hint_sources
            .iter()
            .map(|owner| ResidencyOwner::cache_owner(*owner).compact_key())
            .collect();
        for source in extensions.owner_prefix_blocks.keys() {
            match source {
                KvTransferCandidateSource::Worker(worker) => {
                    assert!(
                        continuations.contains_key(worker),
                        "{ctx}: credited a non-candidate {worker:?}"
                    )
                }
                KvTransferCandidateSource::CacheOwner(key) => {
                    assert!(
                        hint_keys.contains(key),
                        "{ctx}: credited a non-source cache owner"
                    )
                }
            }
        }
        for key in hint_keys {
            let owner = IndexedResidencyOwner::Exact(key);
            let (depth, _, matched) =
                model.walk(&local, LowerTierContinuation::from_root(0), |holder| {
                    holder == owner
                });
            let prefix = credited(KvTransferCandidateSource::CacheOwner(key));
            if depth == 0 {
                assert_eq!(prefix, None, "{ctx}: source off the root edge was credited");
                continue;
            }
            let prefix =
                prefix.unwrap_or_else(|| panic!("{ctx}: source with depth {depth} not credited"));
            assert_eq!(prefix, depth, "{ctx}: source prefix differs from its depth");
            assert_on_chain(&matched, prefix, "hint source");
        }
    }
}

/// Dumped single-block stores must equal the model and rebuild an identical index.
fn check_dump_round_trip(lanes: &[WorkerBlockIndex], model: &Model, ctx: &str) {
    let fresh = LowerTierIndexer::new();
    let mut fresh_lanes: Vec<WorkerBlockIndex> = (0..lanes.len())
        .map(|_| WorkerBlockIndex::default())
        .collect();
    let mut dumped = Memberships::new();
    for lane in lanes {
        for mut event in LowerTierIndexer::dump_events(lane) {
            // Dumps carry a placeholder tier; recovery retags them before replay.
            event.storage_tier = StorageTier::HostPinned;
            let owner = IndexedResidencyOwner::from_exact(event.residency_owner().unwrap());
            let KvCacheEventData::Stored(store) = &event.event.data else {
                panic!("{ctx}: dump emitted a non-store event");
            };
            assert_eq!(
                store.blocks.len(),
                1,
                "{ctx}: dump emitted a multi-block store"
            );
            let block = &store.blocks[0];
            dumped.entry(owner).or_default().insert(
                block.block_hash.0,
                TransitionKey::new(store.parent_hash, block.tokens_hash),
            );
            fresh
                .apply_event(&mut fresh_lanes[lane_of(owner, lanes.len())], event)
                .unwrap();
        }
    }
    assert_eq!(dumped, model.owners, "{ctx}: dump differs from the model");
    let replayed = lane_memberships(&fresh_lanes, ctx);
    assert_eq!(
        replayed, model.owners,
        "{ctx}: replayed dump differs from the model"
    );
    check_edges(&fresh, &replayed, ctx);
}

fn run_case(seed: u64) {
    let mut rng = Rng(seed);
    let prompts = prompts(&mut rng);
    let cache_owners: Vec<CacheOwnerId> = CACHE_OWNER_SLOTS
        .iter()
        .map(|slot| cache_owner(*slot))
        .collect();
    let owners: Vec<SoakOwner> = workers()
        .into_iter()
        .map(SoakOwner::Worker)
        .chain(cache_owners.iter().copied().map(SoakOwner::Cache))
        .collect();
    let lane_count = 1 + rng.below(3);
    let index = LowerTierIndexer::new();
    let mut lanes: Vec<WorkerBlockIndex> = (0..lane_count)
        .map(|_| WorkerBlockIndex::default())
        .collect();
    let mut model = Model::default();

    for op in 0..OPS_PER_CASE {
        let owner = owners[rng.below(owners.len())];
        let indexed = owner.indexed();
        let lane = lane_of(indexed, lane_count);
        let event_id = op as u64;
        let action = rng.below(100);
        let ctx = format!("seed={seed} lanes={lane_count} op={op} owner={owner:?} action={action}");
        match action {
            0..55 => {
                let prompt = &prompts[rng.below(prompts.len())];
                let len = prompt.locals.len();
                let start = rng.below(len);
                let end = start + 1 + rng.below(len - start);
                let hashes = &prompt.hashes;
                let parent = start.checked_sub(1).map(|prev| hashes[prev]);
                let blocks: Vec<(u64, u64)> = (start..end)
                    .map(|pos| (prompt.locals[pos], hashes[pos]))
                    .collect();
                model.store(indexed, parent, &blocks);
                let data = KvCacheEventData::Stored(KvCacheStoreData {
                    parent_hash: parent.map(ExternalSequenceBlockHash),
                    start_position: None,
                    blocks: blocks
                        .iter()
                        .map(|&(local, hash)| KvCacheStoredBlockData {
                            block_hash: ExternalSequenceBlockHash(hash),
                            tokens_hash: LocalBlockHash(local),
                            mm_extra_info: None,
                        })
                        .collect(),
                });
                index
                    .apply_event(&mut lanes[lane], owner.event(event_id, data))
                    .unwrap();
            }
            55..85 => {
                let held: Vec<u64> = model
                    .owners
                    .get(&indexed)
                    .map(|blocks| blocks.keys().copied().collect())
                    .unwrap_or_default();
                let mut hashes: Vec<u64> = held.into_iter().filter(|_| rng.chance(40)).collect();
                if rng.chance(20) {
                    let prompt = &prompts[rng.below(prompts.len())];
                    hashes.push(prompt.hashes[rng.below(prompt.hashes.len())]);
                }
                if rng.chance(10) && !hashes.is_empty() {
                    hashes.push(hashes[0]);
                }
                model.remove(indexed, &hashes);
                let data = KvCacheEventData::Removed(KvCacheRemoveData {
                    block_hashes: hashes.into_iter().map(ExternalSequenceBlockHash).collect(),
                });
                index
                    .apply_event(&mut lanes[lane], owner.event(event_id, data))
                    .unwrap();
            }
            85..93 => {
                model.owners.remove(&indexed);
                index
                    .apply_event(
                        &mut lanes[lane],
                        owner.event(event_id, KvCacheEventData::Cleared),
                    )
                    .unwrap();
            }
            _ => {
                let worker_id = WORKER_IDS[rng.below(WORKER_IDS.len())];
                model.remove_worker(worker_id);
                for lane in &mut lanes {
                    index.remove_worker(lane, worker_id);
                }
            }
        }

        let members = lane_memberships(&lanes, &ctx);
        assert_eq!(members, model.owners, "{ctx}: index differs from the model");
        check_edges(&index, &members, &ctx);
        if op % QUERY_EVERY == QUERY_EVERY - 1 {
            check_queries(&mut rng, &index, &model, &prompts, &cache_owners, &ctx);
        }
    }
    check_dump_round_trip(&lanes, &model, &format!("seed={seed} dump"));
}

fn env_u64(name: &str, default: u64) -> u64 {
    match std::env::var(name) {
        Ok(value) => value
            .parse()
            .unwrap_or_else(|_| panic!("{name} must be a nonnegative decimal integer")),
        Err(std::env::VarError::NotPresent) => default,
        Err(error) => panic!("{name}: {error}"),
    }
}

#[test]
fn lower_tier_index_matches_reference_model_under_random_events() {
    let cases = env_u64("LOWER_TIER_SOAK_CASES", 64);
    let seed = env_u64("LOWER_TIER_SOAK_SEED", 0x10E5_71E5);
    for case in 0..cases {
        run_case(seed.wrapping_add(case));
    }
}

fn store_data(local: &[u64], hashes: &[u64], parent: Option<u64>) -> KvCacheEventData {
    assert_eq!(local.len(), hashes.len());
    KvCacheEventData::Stored(KvCacheStoreData {
        parent_hash: parent.map(ExternalSequenceBlockHash),
        start_position: None,
        blocks: local
            .iter()
            .zip(hashes)
            .map(|(&local, &hash)| KvCacheStoredBlockData {
                block_hash: ExternalSequenceBlockHash(hash),
                tokens_hash: LocalBlockHash(local),
                mm_extra_info: None,
            })
            .collect(),
    })
}

async fn check_merged_sources(seed: u64) {
    let mut rng = Rng(seed);
    let len = 3 + rng.below(6);
    let locals: Vec<u64> = (0..len).map(|_| 1 + rng.below(3) as u64).collect();
    let mut hashes = Vec::with_capacity(len);
    for (pos, &local) in locals.iter().enumerate() {
        let parent = pos.checked_sub(1).map(|prev| hashes[prev]);
        hashes.push(sequence_hash(parent, local));
    }
    // The host extends a device prefix and also contributes independent root sources.
    let device_len = 2 + rng.below(len - 2);

    let device_worker = WorkerWithDpRank::new(7, 0);
    let host_worker = WorkerWithDpRank::new(8, 0);
    let projected_worker = WorkerWithDpRank::new(9, 1);
    let cache_owners = [cache_owner(10), cache_owner(11)];
    let projection = ResidencyProjection::new([(cache_owners[0], projected_worker)]).unwrap();
    let snapshot = ResidencyRoutingSnapshot::new(
        projection.clone(),
        cache_owners.iter().map(|owner| {
            (
                *owner,
                RouterHintSourceMetadata {
                    source_control_endpoint: "tcp://source:9000".to_string(),
                    worker_type: "decode".to_string(),
                    topology_domains: Default::default(),
                },
                projection.cache_owner_worker(*owner),
            )
        }),
    );
    let indexers = LowerTierIndexers::new(1 + rng.below(3), 4);
    indexers.set_residency_routing_snapshot(snapshot);
    let host = indexers.get_or_create(StorageTier::HostPinned);

    // These full source chains are generated independently of both the retained extension
    // positions and the merge output. A source may only advertise its own root prefix.
    let mut source_chains: HashMap<KvTransferCandidateSource, Vec<ExternalSequenceBlockHash>> =
        HashMap::new();
    let owners = [
        (SoakOwner::Worker(host_worker), len),
        (SoakOwner::Worker(projected_worker), 1 + rng.below(len)),
        (SoakOwner::Cache(cache_owners[0]), 1 + rng.below(len)),
        (SoakOwner::Cache(cache_owners[1]), len),
    ];
    for (event_id, &(owner, depth)) in owners.iter().enumerate() {
        host.apply_event_and_wait(owner.event(
            event_id as u64,
            store_data(&locals[..depth], &hashes[..depth], None),
        ))
        .await
        .unwrap();
        let chain: Vec<_> = hashes[..depth]
            .iter()
            .copied()
            .map(ExternalSequenceBlockHash)
            .collect();
        let source = match owner {
            SoakOwner::Worker(worker) => worker.into(),
            SoakOwner::Cache(cache_owner) => KvTransferCandidateSource::CacheOwner(
                ResidencyOwner::cache_owner(cache_owner).compact_key(),
            ),
        };
        source_chains.insert(source, chain.clone());
        if let SoakOwner::Cache(cache_owner) = owner
            && let Some(worker) = projection.cache_owner_worker(cache_owner)
        {
            let projected_chain = source_chains.entry(worker.into()).or_default();
            if projected_chain.len() < chain.len() {
                *projected_chain = chain;
            }
        }
    }
    host.apply_event_and_wait(SoakOwner::Worker(device_worker).event(
        owners.len() as u64,
        store_data(
            &locals[device_len..],
            &hashes[device_len..],
            Some(hashes[device_len - 1]),
        ),
    ))
    .await
    .unwrap();
    source_chains.insert(
        device_worker.into(),
        hashes
            .iter()
            .copied()
            .map(ExternalSequenceBlockHash)
            .collect(),
    );

    let mut overlap_scores = OverlapScores::new();
    overlap_scores
        .scores
        .insert(device_worker, device_len as u32);
    let mut last_matched_hashes = FxHashMap::default();
    last_matched_hashes.insert(
        device_worker,
        ExternalSequenceBlockHash(hashes[device_len - 1]),
    );
    let device_matches = MatchDetails {
        overlap_scores,
        last_matched_hashes,
        kv_transfer_candidates: Some(KvTransferCandidates {
            block_hashes: hashes[..device_len]
                .iter()
                .copied()
                .map(ExternalSequenceBlockHash)
                .collect(),
            owner_prefix_blocks: vec![(device_worker.into(), device_len)],
            source_intervals: Vec::new(),
            routing_snapshot: None,
        }),
    };
    let mut query: Vec<_> = locals.iter().copied().map(LocalBlockHash).collect();
    query.push(LocalBlockHash(99));
    let result = query_lower_tiers_with_options(
        &indexers,
        &query,
        &device_matches,
        LowerTierQueryOptions {
            retain_kv_transfer_chain: true,
        },
    );
    let ctx = format!("seed={seed} device_len={device_len}");
    let candidates = result[&StorageTier::HostPinned]
        .kv_transfer_candidates
        .as_ref()
        .unwrap_or_else(|| panic!("{ctx}: lost device candidates"));
    assert_eq!(
        candidates.block_hashes,
        hashes
            .iter()
            .copied()
            .map(ExternalSequenceBlockHash)
            .collect::<Vec<_>>(),
        "{ctx}: selected chain changed"
    );
    let mut credited = HashMap::new();
    for &(source, depth) in &candidates.owner_prefix_blocks {
        assert!(depth > 0, "{ctx}: zero prefix for {source:?}");
        let chain = source_chains
            .get(&source)
            .unwrap_or_else(|| panic!("{ctx}: unknown source {source:?}"));
        assert!(
            depth <= chain.len(),
            "{ctx}: {source:?} credited past its chain"
        );
        assert!(
            depth <= candidates.block_hashes.len(),
            "{ctx}: {source:?} credited past the retained chain"
        );
        assert_eq!(
            candidates.block_hashes[..depth],
            chain[..depth],
            "{ctx}: {source:?} credited over another source's root-aligned hashes"
        );
        assert!(
            credited.insert(source, depth).is_none(),
            "{ctx}: duplicate source {source:?}"
        );
    }
    for (source, chain) in &source_chains {
        let shared = chain
            .iter()
            .zip(&candidates.block_hashes)
            .take_while(|(own, retained)| own == retained)
            .count();
        assert_eq!(
            credited.get(source).copied().unwrap_or(0),
            shared,
            "{ctx}: {source:?} lost part of its shared prefix"
        );
    }
}

#[tokio::test]
async fn merged_hints_match_each_sources_root_chain() {
    let cases = env_u64("LOWER_TIER_SOAK_CASES", 64);
    let seed = env_u64("LOWER_TIER_SOAK_SEED", 0x10E5_71E5);
    for case in 0..cases {
        check_merged_sources(seed.wrapping_add(case)).await;
    }
}
