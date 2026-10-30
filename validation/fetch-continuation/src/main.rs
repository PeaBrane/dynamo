// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use dynamo_custom_policy_builtin::default_registry;
use dynamo_kv_router::identity::{
    CacheOwnerId, CacheSemanticsId, ClusterId, IdentitySource, IndexerDomainId, PoolId,
    RoutingPartitionId, RoutingScopeId, StableDpSlotId,
};
use dynamo_kv_router::protocols::{
    BlockHashOptions, ExternalSequenceBlockHash, KvCacheEvent, KvCacheEventData, KvCacheStoreData,
    KvCacheStoredBlockData, ResidencyProjection, ResidencyRoutingSnapshot, RouterEvent,
    RouterHintSourceMetadata, StorageTier, compute_block_hash_for_seq, compute_seq_hash_for_block,
};
use dynamo_kv_router::services::indexer::{backend::Indexer, registry::WorkerRegistry};
use dynamo_kv_router::services::selection::{
    KvEventIngress, KvIndexSource, SelectAndReserveRequest, SelectResponse, SelectionService,
    SelectionServiceBuilder,
};
use dynamo_kv_router::{KvRouterConfig, WorkerType};
use serde_json::json;
use std::sync::Arc;

struct FixtureIngress(Indexer);
#[async_trait::async_trait]
impl KvEventIngress for FixtureIngress {
    fn open(&self, _: &WorkerRegistry, _: &RoutingPartitionId, _: u32) -> Indexer {
        self.0.clone()
    }
}

async fn select(
    State(service): State<Arc<SelectionService>>,
    Json(request): Json<SelectAndReserveRequest>,
) -> Result<Json<SelectResponse>, (StatusCode, String)> {
    let response = service
        .select_and_reserve(request)
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    // Every request is completed by this deterministic host, without modifying
    // the fixture inventory. Repetition continues to test the same suffixes.
    if let Some(id) = response.selection_id.as_deref() {
        service
            .free_reservation(id)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    Ok(Json(response))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    anyhow::ensure!(
        args.len() == 3,
        "usage: probe <source-B-endpoint> <source-C-endpoint>"
    );
    let registry = WorkerRegistry::new(2);
    let indexer = registry.get_or_create_indexer(RoutingPartitionId::new("fixture", "default"), 4);
    let service = SelectionServiceBuilder::new(
        KvRouterConfig {
            router_queue_threshold: None,
            ..Default::default()
        },
        WorkerType::Aggregated,
        default_registry(),
    )
    .kv_index(KvIndexSource::Owned(Arc::new(FixtureIngress(
        indexer.clone(),
    ))))
    .build()
    .await?;
    for id in [1, 2] {
        service
            .upsert_worker(serde_json::from_value(json!({
                "worker_id": id, "model_name": "fixture", "routing_group": "default",
                "endpoint": format!("http://fixture-{id}"), "block_size": 4,
                "router_hint_worker_type": "fixture"
            }))?)
            .await?;
    }
    let tokens: Vec<u32> = (0..80).collect();
    let local = compute_block_hash_for_seq(&tokens, 4, BlockHashOptions::default());
    let hashes: Vec<_> = compute_seq_hash_for_block(&local)
        .into_iter()
        .map(ExternalSequenceBlockHash)
        .collect();
    let store = |id, start: usize, end: usize, tier| {
        RouterEvent::with_storage_tier(
            id,
            KvCacheEvent {
                event_id: 1,
                dp_rank: 0,
                data: KvCacheEventData::Stored(KvCacheStoreData {
                    parent_hash: start.checked_sub(1).map(|i| hashes[i]),
                    start_position: None,
                    blocks: (start..end)
                        .map(|i| KvCacheStoredBlockData {
                            block_hash: hashes[i],
                            tokens_hash: local[i],
                            mm_extra_info: None,
                        })
                        .collect(),
                }),
            },
            tier,
        )
    };
    for (id, end) in [(1, 10), (2, 15)] {
        indexer
            .apply_event_routed(store(id, 0, end, StorageTier::Device))
            .await?;
    }
    let mut sources = Vec::new();
    for (id, start, end, endpoint) in [(11, 10, 15, &args[1]), (12, 15, 20, &args[2])] {
        let owner = CacheOwnerId::new(
            PoolId::new(
                IndexerDomainId::new(
                    CacheSemanticsId::new([1; 16], IdentitySource::Explicit),
                    RoutingScopeId::new([2; 16], IdentitySource::Explicit),
                ),
                ClusterId::new(3),
            ),
            StableDpSlotId::new([id as u8; 16], IdentitySource::Explicit),
        );
        let event = store(id, start, end, StorageTier::HostPinned);
        indexer
            .apply_event_routed(RouterEvent::with_cache_owner(
                id,
                event.event,
                StorageTier::HostPinned,
                owner,
            ))
            .await?;
        sources.push((
            owner,
            RouterHintSourceMetadata {
                source_control_endpoint: endpoint.clone(),
                worker_type: "fixture".to_string(),
                topology_domains: Default::default(),
            },
            None,
        ));
    }
    indexer.set_residency_routing_snapshot(ResidencyRoutingSnapshot::new(
        ResidencyProjection::default(),
        sources,
    ));
    let dump = indexer.dump_events().await?;
    anyhow::ensure!(
        dump.iter().filter(|e| e.state_source.is_some()).count() == 10,
        "unexpected source inventory dump"
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let mut shutdown = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    println!(
        "{}",
        json!({"url": format!("http://{}", listener.local_addr()?), "hashes": hashes, "tokens": tokens})
    );
    let service = Arc::new(service);
    let app = Router::new()
        .route("/select_and_reserve", post(select))
        .with_state(service.clone());
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown.recv().await;
        })
        .await?;
    service.shutdown().await;
    Ok(())
}
