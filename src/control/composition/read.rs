use std::{
    collections::HashSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use bytes::Bytes;
use composite_mvt::{Compression, MvtSource};
use futures_util::{
    future::BoxFuture,
    stream::{FuturesUnordered, StreamExt},
};
use tokio::sync::Semaphore;

use super::{CompiledComposite, CompositeLimits, Member, MemberFailurePolicy, SourceSlot};
use crate::{
    core::response::{MemberFailure, TileOutcome},
    error::{ErrorKind, MaptileError, MaptileResult},
    sources::{TileCoord, TileEncoding, TileFormat, TileQuery, TileResponse},
    transport,
};

#[derive(Clone)]
struct ReadBudget(Arc<BudgetState>);
struct BudgetState {
    limits: CompositeLimits,
    semaphore: Semaphore,
    decode_lock: Mutex<()>,
    compressed: AtomicUsize,
    decoded: AtomicUsize,
    work: AtomicUsize,
}
impl ReadBudget {
    fn new(limits: CompositeLimits) -> Self {
        Self(Arc::new(BudgetState {
            semaphore: Semaphore::new(limits.max_concurrency.clamp(1, Semaphore::MAX_PERMITS)),
            decode_lock: Mutex::new(()),
            compressed: AtomicUsize::new(0),
            decoded: AtomicUsize::new(0),
            work: AtomicUsize::new(0),
            limits,
        }))
    }
    fn charge(counter: &AtomicUsize, amount: usize, limit: usize) -> MaptileResult<()> {
        counter
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                n.checked_add(amount).filter(|&x| x <= limit)
            })
            .map(|_| ())
            .map_err(|_| MaptileError::PayloadTooLarge { limit })
    }
    fn reserve(&self, amount: usize) -> MaptileResult<Work> {
        Self::charge(&self.work, amount, self.limits.max_work_bytes)?;
        Ok(Work {
            budget: self.clone(),
            amount,
        })
    }
}
impl std::ops::Deref for ReadBudget {
    type Target = BudgetState;
    fn deref(&self) -> &BudgetState {
        &self.0
    }
}
struct Work {
    budget: ReadBudget,
    amount: usize,
}
impl Drop for Work {
    fn drop(&mut self) {
        self.budget.work.fetch_sub(self.amount, Ordering::AcqRel);
    }
}

impl CompiledComposite {
    pub(in crate::control) async fn read(
        &self,
        coord: TileCoord,
        query: Option<&TileQuery<'_>>,
        limits: CompositeLimits,
    ) -> MaptileResult<TileOutcome> {
        self.read_inner(coord, query, &ReadBudget::new(limits))
            .await
            .map(|r| r.outcome)
    }
    fn read_inner<'a>(
        &'a self,
        coord: TileCoord,
        query: Option<&'a TileQuery<'a>>,
        budget: &'a ReadBudget,
    ) -> BoxFuture<'a, MaptileResult<Held>> {
        Box::pin(async move {
            let composer = self
                .composer
                .as_ref()
                .ok_or_else(|| MaptileError::SourceUnavailable(self.descriptor.id.to_string()))?;
            let mut reads = FuturesUnordered::new();
            for (index, member) in self.members.iter().enumerate() {
                reads.push(async move {
                    let result = match member {
                        Member::Source(slot) => read_leaf(slot, coord, query, budget).await,
                        Member::Composite(node) => node.read_inner(coord, query, budget).await,
                    };
                    (index, member, result)
                });
            }
            let mut raw = vec![Bytes::new(); self.members.len()];
            let mut reservations = Vec::new();
            let mut failure_slots = vec![Vec::new(); self.members.len()];
            let mut found = false;
            let mut success = 0usize;
            let mut last_modified = None;
            while let Some((index, member, result)) = reads.next().await {
                let held = match result {
                    Ok(held) => {
                        success += 1;
                        held
                    }
                    Err(error)
                        if self.policy == MemberFailurePolicy::SkipTransient
                            && matches!(
                                error.kind(),
                                ErrorKind::Unavailable | ErrorKind::Deadline
                            ) =>
                    {
                        tracing::warn!(target: "maptile_core::control::composite", source_id = %member.descriptor().id, reason = ?error.kind(), "composite member unavailable");
                        record_failure(member.descriptor().id.as_ref(), error.kind());
                        failure_slots[index].push(MemberFailure {
                            source: member.descriptor().id.clone(),
                            error,
                        });
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                reservations.extend(held.work);
                let tile = match held.outcome {
                    TileOutcome::NoTile => None,
                    TileOutcome::Present(tile) => Some(tile),
                    TileOutcome::Partial {
                        tile,
                        failures: nested,
                    } => {
                        failure_slots[index] = nested;
                        tile
                    }
                };
                if let Some(tile) = tile {
                    found = true;
                    last_modified = match (last_modified, tile.last_modified) {
                        (Some(a), Some(b)) => Some(std::cmp::max(a, b)),
                        (a, b) => a.or(b),
                    };
                    raw[index] = tile.data;
                }
            }
            let mut failures: Vec<_> = failure_slots.into_iter().flatten().collect();
            if success == 0 && !failures.is_empty() {
                return Err(failures.remove(0).error);
            }
            if !found {
                return Ok(Held {
                    outcome: if failures.is_empty() {
                        TileOutcome::NoTile
                    } else {
                        TileOutcome::Partial {
                            tile: None,
                            failures,
                        }
                    },
                    work: reservations,
                });
            }
            let total = raw
                .iter()
                .try_fold(0usize, |n, b| n.checked_add(b.len()))
                .ok_or(MaptileError::PayloadTooLarge {
                    limit: budget.limits.max_decoded_bytes,
                })?;
            let limit = budget
                .limits
                .max_decoded_bytes
                .min(transport::MAX_DECODED_TILE_BYTES);
            if total > limit {
                return Err(MaptileError::PayloadTooLarge { limit });
            }
            let output_work = budget.reserve(total)?;
            let composer = composer.clone();
            let (data, output_work) = transport::run_blocking(move || {
                let data = composer
                    .compose(&raw)
                    .map_err(|e| MaptileError::Transcode(format!("composite: {e}")))?;
                drop(reservations);
                Ok((data, output_work))
            })
            .await?;
            if data.len() > limit {
                return Err(MaptileError::PayloadTooLarge { limit });
            }
            let tile = TileResponse {
                data,
                info: self.descriptor.info,
                last_modified,
            };
            Ok(Held {
                outcome: if failures.is_empty() {
                    TileOutcome::Present(tile)
                } else {
                    TileOutcome::Partial {
                        tile: Some(tile),
                        failures,
                    }
                },
                work: vec![output_work],
            })
        })
    }
}
struct Held {
    outcome: TileOutcome,
    work: Vec<Work>,
}

async fn read_leaf(
    slot: &SourceSlot,
    coord: TileCoord,
    query: Option<&TileQuery<'_>>,
    budget: &ReadBudget,
) -> MaptileResult<Held> {
    let _permit = budget
        .semaphore
        .acquire()
        .await
        .map_err(MaptileError::backend)?;
    let outcome = slot.read(coord, query).await?;
    let mut tile = match outcome {
        TileOutcome::Present(tile) => tile,
        TileOutcome::NoTile => {
            return Ok(Held {
                outcome: TileOutcome::NoTile,
                work: Vec::new(),
            });
        }
        TileOutcome::Partial { .. } => {
            return Err(MaptileError::InvalidMetadata(
                "leaf returned a partial composite result".into(),
            ));
        }
    };
    if tile.info.format != TileFormat::Mvt {
        return Err(MaptileError::InvalidMetadata(
            "composite member returned non-MVT tile".into(),
        ));
    }
    ReadBudget::charge(
        &budget.compressed,
        tile.data.len(),
        budget.limits.max_compressed_bytes,
    )?;
    let encoded_work = budget.reserve(tile.data.len())?;
    let descriptor = slot.descriptor().clone();
    let budget = budget.clone();
    transport::run_blocking(move || {
        let _lock = budget
            .decode_lock
            .lock()
            .map_err(|_| MaptileError::BackendMsg("composite decode lock poisoned".into()))?;
        let remaining = budget
            .limits
            .max_decoded_bytes
            .min(transport::MAX_DECODED_TILE_BYTES)
            .saturating_sub(budget.decoded.load(Ordering::Acquire));
        let work_remaining = budget
            .limits
            .max_work_bytes
            .saturating_sub(budget.work.load(Ordering::Acquire));
        let raw = transport::decode_with_limit(
            &tile.data,
            tile.info.encoding,
            remaining.min(work_remaining),
        )?;
        ReadBudget::charge(
            &budget.decoded,
            raw.len(),
            budget
                .limits
                .max_decoded_bytes
                .min(transport::MAX_DECODED_TILE_BYTES),
        )?;
        let raw_work = budget.reserve(raw.len())?;
        if !raw.is_empty() {
            let parsed = MvtSource::from_mvt_with_compression(
                descriptor.id.to_string(),
                &raw,
                Compression::None,
            )
            .map_err(|e| MaptileError::InvalidMetadata(format!("invalid MVT: {e}")))?;
            let declared = descriptor
                .tilejson
                .vector_layers
                .as_ref()
                .expect("validated at build");
            let mut seen = HashSet::new();
            for layer in parsed.layers() {
                let name = layer.as_ref();
                if !seen.insert(name) || !declared.iter().any(|d| d.id == name) {
                    return Err(MaptileError::InvalidMetadata(format!(
                        "unexpected or duplicate layer {name} in {}",
                        descriptor.id
                    )));
                }
            }
        }
        tile.data = raw;
        tile.info.encoding = TileEncoding::Uncompressed;
        drop(encoded_work);
        Ok(Held {
            outcome: TileOutcome::Present(tile),
            work: vec![raw_work],
        })
    })
    .await
}

#[cfg(feature = "metrics")]
fn record_failure(source: &str, kind: ErrorKind) {
    use std::sync::LazyLock;
    static FAILURES: LazyLock<prometheus::IntCounterVec> = LazyLock::new(|| {
        prometheus::register_int_counter_vec!(
            "maptile_composite_member_failures_total",
            "Skipped transient composite member failures",
            &["source", "reason"]
        )
        .expect("metric registration")
    });
    FAILURES
        .with_label_values(&[source, &format!("{kind:?}")])
        .inc();
}
#[cfg(not(feature = "metrics"))]
fn record_failure(_source: &str, _kind: ErrorKind) {}
