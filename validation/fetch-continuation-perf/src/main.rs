// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Fixed-inventory cost probe. No fetches, request queues, or inference are simulated.
use dynamo_kv_router::identity::{
    CacheOwnerId, CacheSemanticsId, ClusterId, IdentitySource, IndexerDomainId, PoolId,
    RoutingScopeId, StableDpSlotId,
};
use dynamo_kv_router::indexer::{
    LowerTierIndexers, LowerTierMatchDetails, LowerTierQueryOptions, MatchDetails,
    query_lower_tiers_with_options,
};
use dynamo_kv_router::kv_hints::{
    KvTransferCandidateSource, KvTransferCandidates, KvTransferScope, KvTransferSourceChoice,
    KvTransferSourceSummary, WorkerKvTransferKeys, WorkerKvTransferKeysCache,
};
use dynamo_kv_router::protocols::{
    ExternalSequenceBlockHash, KvCacheEvent, KvCacheEventData, KvCacheStoreData,
    KvCacheStoredBlockData, KvHintTransferWorkerMetadata, LocalBlockHash, ResidencyOwner,
    ResidencyProjection, ResidencyRoutingSnapshot, RouterEvent, RouterHintSourceMetadata,
    StorageTier, WorkerConfigLike, WorkerWithDpRank,
};
use serde::Serialize;
use std::borrow::Cow;
use std::collections::HashMap;
use std::hint::black_box;
#[cfg(not(feature = "allocations"))]
use std::time::{Duration, Instant};

#[cfg(feature = "allocations")]
mod counted {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;
    thread_local! {
        static ACTIVE: Cell<bool> = const { Cell::new(false) };
        static CALLS: Cell<u64> = const { Cell::new(0) };
        static BYTES: Cell<u64> = const { Cell::new(0) };
    }
    pub struct Allocator;
    fn record(bytes: usize) {
        if ACTIVE.get() {
            CALLS.set(CALLS.get() + 1);
            BYTES.set(BYTES.get() + bytes as u64);
        }
    }
    // Delegates all allocation semantics to System; only this thread is counted.
    unsafe impl GlobalAlloc for Allocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            record(layout.size());
            unsafe { System.alloc(layout) }
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            record(layout.size());
            unsafe { System.alloc_zeroed(layout) }
        }
        unsafe fn realloc(&self, p: *mut u8, layout: Layout, n: usize) -> *mut u8 {
            record(n);
            unsafe { System.realloc(p, layout, n) }
        }
        unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
            unsafe { System.dealloc(p, layout) }
        }
    }
    pub fn measure(mut f: impl FnMut()) -> (u64, u64) {
        CALLS.set(0);
        BYTES.set(0);
        ACTIVE.set(true);
        for _ in 0..16 {
            f();
        }
        ACTIVE.set(false);
        assert_eq!(CALLS.get() % 16, 0);
        assert_eq!(BYTES.get() % 16, 0);
        (CALLS.get() / 16, BYTES.get() / 16)
    }
}
#[cfg(feature = "allocations")]
#[global_allocator]
static ALLOCATOR: counted::Allocator = counted::Allocator;

#[derive(Clone, Copy, Serialize)]
struct Case {
    name: &'static str,
    blocks: usize,
    owners: usize,
    targets: usize,
    shape: &'static str,
    islands: usize,
}
fn cases() -> Vec<Case> {
    [
        ("4k_shared", 256, 8, 8, "suffix", 1),
        ("32k_no_sources", 2048, 0, 8, "suffix", 1),
        ("32k_single", 2048, 1, 8, "suffix", 1),
        ("32k_pair", 2048, 2, 8, "suffix", 1),
        ("32k_shared", 2048, 8, 8, "suffix", 1),
        ("32k_64owners", 2048, 64, 8, "suffix", 1),
        ("32k_256owners", 2048, 256, 8, "suffix", 1),
        ("32k_64targets", 2048, 64, 64, "suffix", 1),
        ("32k_root", 2048, 64, 8, "root", 1),
        ("32k_staggered", 2048, 64, 8, "staggered", 1),
        ("32k_fragmented", 2048, 64, 8, "fragmented", 1),
        ("32k_fully_local", 2048, 64, 8, "local_fragmented", 1),
        ("32k_islands", 2048, 64, 64, "staggered", 8),
        ("128k_shared", 8192, 256, 64, "suffix", 1),
        ("128k_fragmented", 8192, 64, 64, "fragmented", 1),
    ]
    .into_iter()
    .map(|(name, blocks, owners, targets, shape, islands)| Case {
        name,
        blocks,
        owners,
        targets,
        shape,
        islands,
    })
    .collect()
}

struct Worker {
    topology: HashMap<String, String>,
    islands: bool,
    keys: WorkerKvTransferKeysCache,
}
impl WorkerConfigLike for Worker {
    fn data_parallel_start_rank(&self) -> u32 {
        0
    }
    fn data_parallel_size(&self) -> u32 {
        1
    }
    fn max_num_batched_tokens(&self) -> Option<u64> {
        None
    }
    fn total_kv_blocks(&self) -> Option<u64> {
        Some(65536)
    }
    fn kv_hint_transfer_metadata_for_dp_rank(
        &self,
        _: u32,
    ) -> Option<KvHintTransferWorkerMetadata<'_>> {
        // Destinations consume hints; exact cache owners are the fetch sources.
        Some(KvHintTransferWorkerMetadata {
            worker_type: "prefill",
            source_control_endpoint: None,
        })
    }
    fn topology_domains(&self) -> Option<&HashMap<String, String>> {
        Some(&self.topology)
    }
    fn kv_transfer_domain(&self) -> Option<&str> {
        self.islands.then_some("island")
    }
    fn kv_transfer_keys(&self) -> Cow<'_, WorkerKvTransferKeys> {
        Cow::Borrowed(self.keys.get_or_init(self))
    }
}
struct Source {
    source: KvTransferCandidateSource,
    present: Vec<bool>,
    island: usize,
    attached: Option<WorkerWithDpRank>,
}
struct Fixture {
    indexers: LowerTierIndexers,
    sequence: Vec<LocalBlockHash>,
    hashes: Vec<ExternalSequenceBlockHash>,
    device: MatchDetails,
    workers: HashMap<u64, Worker>,
    targets: Vec<(WorkerWithDpRank, usize)>,
    sources: Vec<Source>,
    case: Case,
}
fn owner(id: usize) -> CacheOwnerId {
    CacheOwnerId::new(
        PoolId::new(
            IndexerDomainId::new(
                CacheSemanticsId::new([1; 16], IdentitySource::Explicit),
                RoutingScopeId::new([2; 16], IdentitySource::Explicit),
            ),
            ClusterId::new(3),
        ),
        StableDpSlotId::new((id as u128 + 1).to_le_bytes(), IdentitySource::Explicit),
    )
}
fn ranges(present: &[bool]) -> Vec<(usize, usize)> {
    let mut result = Vec::new();
    let mut p = 0;
    while p < present.len() {
        if !present[p] {
            p += 1;
            continue;
        }
        let start = p;
        while p < present.len() && present[p] {
            p += 1;
        }
        result.push((start, p));
    }
    result
}
impl Fixture {
    async fn new(case: Case) -> Self {
        let sequence: Vec<_> = (0..case.blocks)
            .map(|p| LocalBlockHash(p as u64 + 1))
            .collect();
        let hashes: Vec<_> = (0..case.blocks)
            .map(|p| ExternalSequenceBlockHash(p as u64 + 10001))
            .collect();
        let targets: Vec<_> = (0..case.targets)
            .map(|i| {
                (
                    WorkerWithDpRank::new(i as u64 + 1, 0),
                    if case.shape == "local_fragmented" {
                        case.blocks
                    } else {
                        case.blocks / 2 + (i % 4) * case.blocks / 16
                    },
                )
            })
            .collect();
        let workers = targets
            .iter()
            .enumerate()
            .map(|(i, (w, _))| {
                (
                    w.worker_id,
                    Worker {
                        topology: [("island".into(), format!("{}", i % case.islands))].into(),
                        islands: case.islands > 1,
                        keys: Default::default(),
                    },
                )
            })
            .collect();
        let mut device = MatchDetails::default();
        for &(w, depth) in &targets {
            device.overlap_scores.scores.insert(w, depth as u32);
            device.last_matched_hashes.insert(w, hashes[depth - 1]);
        }
        device.retain_kv_transfer_candidates(
            hashes[..targets.iter().map(|t| t.1).max().unwrap()].to_vec(),
        );
        let indexers = LowerTierIndexers::new(1, 16);
        let host = indexers.get_or_create(StorageTier::HostPinned);
        let mut sources = Vec::new();
        let mut metadata = Vec::new();
        for i in 0..case.owners {
            let present: Vec<_> = (0..case.blocks)
                .map(|p| match case.shape {
                    "root" => true,
                    "suffix" => p >= case.blocks / 2,
                    "staggered" => {
                        p >= case.blocks / 4 + i * (case.blocks / 2) / case.owners
                            && p < case.blocks - (i % 4) * case.blocks / 32
                    }
                    "fragmented" | "local_fragmented" => {
                        p >= case.blocks / 4 && (p / 16 + i) % 2 == 0
                    }
                    _ => unreachable!(),
                })
                .collect();
            let owner = owner(i);
            let attached = (i % 4 == 0).then_some(targets[i % targets.len()].0);
            metadata.push((
                owner,
                RouterHintSourceMetadata {
                    source_control_endpoint: format!("tcp://source-{i}.invalid:9000"),
                    worker_type: "prefill".into(),
                    topology_domains: [("island".into(), format!("{}", i % case.islands))].into(),
                },
                attached,
            ));
            for (event_id, (start, end)) in ranges(&present).into_iter().enumerate() {
                host.apply_event_and_wait(RouterEvent::with_cache_owner(
                    1000 + i as u64,
                    KvCacheEvent {
                        event_id: event_id as u64,
                        dp_rank: 0,
                        data: KvCacheEventData::Stored(KvCacheStoreData {
                            parent_hash: start.checked_sub(1).map(|p| hashes[p]),
                            start_position: None,
                            blocks: (start..end)
                                .map(|p| KvCacheStoredBlockData {
                                    block_hash: hashes[p],
                                    tokens_hash: sequence[p],
                                    mm_extra_info: None,
                                })
                                .collect(),
                        }),
                    },
                    StorageTier::HostPinned,
                    owner,
                ))
                .await
                .unwrap();
            }
            sources.push(Source {
                source: KvTransferCandidateSource::CacheOwner(
                    ResidencyOwner::cache_owner(owner).compact_key(),
                ),
                present,
                island: i % case.islands,
                attached,
            });
        }
        indexers.set_residency_routing_snapshot(ResidencyRoutingSnapshot::new(
            ResidencyProjection::default(),
            metadata,
        ));
        Self {
            indexers,
            sequence,
            hashes,
            device,
            workers,
            targets,
            sources,
            case,
        }
    }
    fn lookup(&self, enabled: bool) -> HashMap<StorageTier, LowerTierMatchDetails> {
        let options = LowerTierQueryOptions {
            retain_kv_transfer_chain: true,
            #[cfg(all(feature = "continuation", not(feature = "standard")))]
            fetch_continuation: enabled,
        };
        #[cfg(not(feature = "continuation"))]
        assert!(!enabled);
        #[cfg(feature = "standard")]
        assert!(enabled);
        query_lower_tiers_with_options(&self.indexers, &self.sequence, &self.device, options)
    }
    fn summary(&self, candidates: &KvTransferCandidates) -> KvTransferSourceSummary {
        let scope = KvTransferScope::of(self.targets.iter().map(|(w, depth)| {
            (
                self.workers[&w.worker_id]
                    .kv_transfer_keys()
                    .target()
                    .unwrap(),
                *depth,
            )
        }));
        KvTransferSourceSummary::new(candidates, &self.workers, scope)
    }
    fn choose(&self, summary: &KvTransferSourceSummary) {
        for &(w, depth) in &self.targets {
            black_box(
                summary.source_for(
                    w,
                    self.workers[&w.worker_id]
                        .kv_transfer_keys()
                        .target()
                        .unwrap(),
                    depth,
                ),
            );
        }
    }
    fn pipeline(&self, enabled: bool) {
        let mut lookup = self.lookup(enabled);
        let candidates = if cfg!(feature = "standard") {
            lookup
                .get_mut(&StorageTier::HostPinned)
                .unwrap()
                .kv_transfer_candidates
                .take()
                .unwrap()
        } else {
            lookup[&StorageTier::HostPinned]
                .kv_transfer_candidates
                .as_ref()
                .unwrap()
                .clone()
        };
        drop(lookup);
        self.choose(&self.summary(&candidates));
        black_box(candidates);
    }
    fn validate(&self, enabled: bool, candidates: &KvTransferCandidates) -> serde_json::Value {
        assert_eq!(
            candidates.block_hashes,
            self.hashes[..candidates.block_hashes.len()]
        );
        let summary = self.summary(candidates);
        let mut choices = Vec::new();
        for (target_index, &(w, depth)) in self.targets.iter().enumerate() {
            // Independent bitmap oracle: scan only the original local frontier.
            let expected = self
                .sources
                .iter()
                .filter(|s| {
                    s.attached != Some(w)
                        && (self.case.islands == 1 || s.island == target_index % self.case.islands)
                })
                .filter_map(|s| {
                    if !enabled && !s.present[..depth].iter().all(|&b| b) {
                        return None;
                    }
                    let end = (depth..self.case.blocks)
                        .find(|&p| !s.present[p])
                        .unwrap_or(self.case.blocks);
                    (end > depth).then_some(KvTransferSourceChoice {
                        source: s.source,
                        prefix_blocks: end,
                    })
                })
                .max_by_key(|c| (c.prefix_blocks, std::cmp::Reverse(c.source)));
            let actual = summary.source_for(
                w,
                self.workers[&w.worker_id]
                    .kv_transfer_keys()
                    .target()
                    .unwrap(),
                depth,
            );
            assert_eq!(actual, expected, "{} target {target_index}", self.case.name);
            choices.push(actual.map(|c| {
                (
                    self.sources
                        .iter()
                        .position(|s| s.source == c.source)
                        .unwrap(),
                    c.prefix_blocks,
                )
            }));
        }
        #[cfg(feature = "continuation")]
        let interval_count = {
            let mut actual: Vec<_> = candidates
                .source_intervals
                .iter()
                .map(|i| (i.source, i.start, i.end))
                .collect();
            let mut expected: Vec<_> = if enabled {
                self.sources
                    .iter()
                    .flat_map(|s| {
                        ranges(&s.present)
                            .into_iter()
                            .filter(|&(start, end)| {
                                start > 0
                                    && (!cfg!(feature = "standard")
                                        || self
                                            .targets
                                            .iter()
                                            .any(|&(_, depth)| start <= depth && depth < end))
                            })
                            .map(|(start, end)| (s.source, start, end))
                            .collect::<Vec<_>>()
                    })
                    .collect()
            } else {
                Vec::new()
            };
            actual.sort_unstable();
            expected.sort_unstable();
            assert_eq!(actual, expected, "{} intervals", self.case.name);
            actual.len()
        };
        #[cfg(not(feature = "continuation"))]
        let interval_count = 0;
        #[cfg(not(feature = "standard"))]
        let off = self.lookup(false);
        #[cfg(feature = "standard")]
        let off = query_lower_tiers_with_options(
            &self.indexers,
            &self.sequence,
            &self.device,
            LowerTierQueryOptions {
                retain_kv_transfer_chain: false,
            },
        );
        let current = self.lookup(enabled);
        assert_eq!(
            off[&StorageTier::HostPinned].hits,
            current[&StorageTier::HostPinned].hits
        );
        assert_eq!(
            off[&StorageTier::HostPinned].next_continuations,
            current[&StorageTier::HostPinned].next_continuations
        );
        serde_json::json!({"choices": choices, "intervals": interval_count,
            "root_candidates": candidates.owner_prefix_blocks.len(), "hashes": candidates.block_hashes.len(),
            "memberships": self.sources.iter().map(|s| s.present.iter().filter(|&&b| b).count()).sum::<usize>()})
    }
}

#[cfg(not(feature = "allocations"))]
#[derive(Serialize)]
struct Timing {
    samples: usize,
    mean_ns: f64,
    p50_ns: u64,
    p99_ns: u64,
}
#[cfg(not(feature = "allocations"))]
fn measure(mut f: impl FnMut(), ms: u64) -> Timing {
    let warm = Instant::now();
    while warm.elapsed() < Duration::from_millis(30) {
        f();
    }
    let mut samples = Vec::with_capacity(1_000_000);
    let begin = Instant::now();
    while (begin.elapsed() < Duration::from_millis(ms) || samples.len() < 256)
        && samples.len() < samples.capacity()
    {
        let t = Instant::now();
        f();
        samples.push(t.elapsed().as_nanos() as u64);
    }
    samples.sort_unstable();
    Timing {
        samples: samples.len(),
        mean_ns: samples.iter().map(|&v| v as f64).sum::<f64>() / samples.len() as f64,
        p50_ns: samples[samples.len() / 2],
        p99_ns: samples[(samples.len() - 1) * 99 / 100],
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<_> = std::env::args().collect();
    assert!(
        args.len() >= 3,
        "usage: fetch-continuation-perf off|on OUTPUT [case|all] [ms]"
    );
    let enabled = match args[1].as_str() {
        "off" => false,
        "on" => true,
        _ => panic!("mode must be off|on"),
    };
    let selected = args.get(3).map(String::as_str).unwrap_or("all");
    let ms = args
        .get(4)
        .map(|s| s.parse::<u64>().unwrap())
        .unwrap_or(250);
    let mut results = Vec::new();
    for case in cases()
        .into_iter()
        .filter(|c| selected == "all" || c.name == selected)
    {
        let f = Fixture::new(case).await;
        let lookup = f.lookup(enabled);
        let candidates = lookup[&StorageTier::HostPinned]
            .kv_transfer_candidates
            .as_ref()
            .unwrap();
        let validation = f.validate(enabled, candidates);
        #[cfg(not(feature = "allocations"))]
        let metrics = serde_json::json!({
            "lookup": measure(|| { black_box(f.lookup(enabled)); }, ms),
            "ranking": measure(|| f.choose(&f.summary(candidates)), ms),
            "pipeline": measure(|| f.pipeline(enabled), ms),
        });
        #[cfg(feature = "allocations")]
        let metrics = {
            let _ = ms;
            f.pipeline(enabled);
            let (calls, bytes) = counted::measure(|| f.pipeline(enabled));
            serde_json::json!({"allocations": calls, "allocated_bytes": bytes})
        };
        results
            .push(serde_json::json!({"case": case, "validation": validation, "metrics": metrics}));
    }
    assert!(!results.is_empty(), "unknown case");
    std::fs::write(&args[2], serde_json::to_vec_pretty(&serde_json::json!({
        "schema": 1, "standard": cfg!(feature = "standard"), "continuation_compiled": cfg!(feature = "continuation"), "enabled": enabled,
        "counting_allocator": cfg!(feature = "allocations"), "block_size": 16, "results": results,
    })).unwrap()).unwrap();
}
