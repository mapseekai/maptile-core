use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use bytes::Bytes;
use composite_mvt::{Compression, MvtComposer, MvtSource};
use futures_util::{
    future::BoxFuture,
    stream::{FuturesUnordered, StreamExt},
};
use tokio::sync::Semaphore;

use crate::{
    cache::{CacheZoomRange, ContentVersion},
    control::{SourceCapabilities, SourceDescriptor, SourceSlot},
    error::{ErrorKind, MaptileError, MaptileResult},
    service::{MemberFailure, TileOutcome},
    sources::{SourceKind, TileCoord, TileEncoding, TileFormat, TileInfo, TileQuery, TileResponse},
    transport,
};

pub use composite_mvt::DuplicateLayer;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MemberFailurePolicy {
    #[default]
    FailFast,
    SkipTransient,
}

#[derive(Clone, Debug)]
pub struct CompositeDeclaration {
    pub id: Arc<str>,
    pub members: Vec<Arc<str>>,
    pub lazy: bool,
    pub duplicate_layer: DuplicateLayer,
    pub failure_policy: MemberFailurePolicy,
}

#[derive(Clone, Copy, Debug)]
pub struct CompositeLimits {
    pub max_depth: usize,
    pub max_leaves: usize,
    pub max_concurrency: usize,
    pub max_compressed_bytes: usize,
    pub max_decoded_bytes: usize,
    pub max_work_bytes: usize,
}
impl Default for CompositeLimits {
    fn default() -> Self {
        Self {
            max_depth: 64,
            max_leaves: 128,
            max_concurrency: 8,
            max_compressed_bytes: 16 * 1024 * 1024,
            max_decoded_bytes: 16 * 1024 * 1024,
            max_work_bytes: 64 * 1024 * 1024,
        }
    }
}

#[derive(Clone)]
pub(crate) struct ReadBudget(Arc<BudgetState>);
pub(crate) struct BudgetState {
    limits: CompositeLimits,
    semaphore: Semaphore,
    decode_lock: Mutex<()>,
    compressed: AtomicUsize,
    decoded: AtomicUsize,
    work: AtomicUsize,
}
impl ReadBudget {
    pub(crate) fn new(limits: CompositeLimits) -> Self {
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

#[derive(Clone)]
enum Member {
    Source(Arc<SourceSlot>),
    Composite(Arc<CompiledComposite>),
}
impl Member {
    fn descriptor(&self) -> &SourceDescriptor {
        match self {
            Self::Source(s) => s.descriptor(),
            Self::Composite(c) => c.descriptor(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct CompiledComposite {
    descriptor: SourceDescriptor,
    leaves: Box<[Arc<SourceSlot>]>,
    members: Box<[Member]>,
    composer: Option<Arc<MvtComposer>>,
    policy: MemberFailurePolicy,
}
impl CompiledComposite {
    pub(crate) fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }
    pub(crate) fn leaves(&self) -> &[Arc<SourceSlot>] {
        &self.leaves
    }
    pub(crate) async fn read(
        &self,
        coord: TileCoord,
        query: Option<&TileQuery<'_>>,
        budget: &ReadBudget,
    ) -> MaptileResult<TileOutcome> {
        self.read_inner(coord, query, budget)
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
                        tracing::warn!(source_id = %member.descriptor().id, reason = ?error.kind(), "composite member unavailable");
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

pub(crate) fn compile_graph(
    sources: &HashMap<Arc<str>, Arc<SourceSlot>>,
    declarations: &[CompositeDeclaration],
    limits: CompositeLimits,
) -> MaptileResult<HashMap<Arc<str>, Arc<CompiledComposite>>> {
    if limits.max_depth == 0
        || limits.max_depth > 64
        || limits.max_leaves == 0
        || limits.max_concurrency == 0
        || limits.max_concurrency > Semaphore::MAX_PERMITS
        || limits.max_decoded_bytes == 0
        || limits.max_compressed_bytes == 0
        || limits.max_work_bytes == 0
    {
        return Err(MaptileError::Config("invalid composite limits".into()));
    }
    let mut defs = HashMap::new();
    for decl in declarations {
        for member in &decl.members {
            super::validate_id(member)?;
        }
        if sources.contains_key(&decl.id) || defs.insert(decl.id.clone(), decl).is_some() {
            return Err(MaptileError::Config(format!(
                "duplicate or empty composite id {}",
                decl.id
            )));
        }
        if decl.members.is_empty() || decl.duplicate_layer == DuplicateLayer::Allow {
            return Err(MaptileError::Config(format!(
                "invalid members or duplicate policy for {}",
                decl.id
            )));
        }
    }
    fn check(
        id: &Arc<str>,
        defs: &HashMap<Arc<str>, &CompositeDeclaration>,
        active: &mut HashSet<Arc<str>>,
        done: &mut HashMap<Arc<str>, usize>,
        depth_now: usize,
        limit: usize,
    ) -> MaptileResult<usize> {
        if depth_now > limit {
            return Err(MaptileError::Config("composite graph too deep".into()));
        }
        if active.contains(id) {
            return Err(MaptileError::Config(format!("composite cycle at {id}")));
        }
        if let Some(depth) = done.get(id) {
            if depth_now + depth - 1 > limit {
                return Err(MaptileError::Config("composite graph too deep".into()));
            }
            return Ok(*depth);
        }
        if let Some(decl) = defs.get(id) {
            active.insert(id.clone());
            let mut depth = 1;
            for child in &decl.members {
                if defs.contains_key(child) {
                    depth = depth.max(1 + check(child, defs, active, done, depth_now + 1, limit)?);
                }
            }
            active.remove(id);
            if depth > limit {
                return Err(MaptileError::Config("composite graph too deep".into()));
            }
            done.insert(id.clone(), depth);
            return Ok(depth);
        }
        Ok(0)
    }
    let mut done = HashMap::new();
    for id in defs.keys() {
        check(
            id,
            &defs,
            &mut HashSet::new(),
            &mut done,
            1,
            limits.max_depth,
        )?;
    }
    let mut built = HashMap::new();
    for id in defs.keys() {
        build_node(id, sources, &defs, &mut built, limits)?;
    }
    Ok(built)
}

fn build_node(
    id: &Arc<str>,
    sources: &HashMap<Arc<str>, Arc<SourceSlot>>,
    defs: &HashMap<Arc<str>, &CompositeDeclaration>,
    built: &mut HashMap<Arc<str>, Arc<CompiledComposite>>,
    limits: CompositeLimits,
) -> MaptileResult<Arc<CompiledComposite>> {
    if let Some(node) = built.get(id) {
        return Ok(node.clone());
    }
    let decl = defs[id];
    let mut members = Vec::new();
    let mut leaves = Vec::new();
    let mut leaf_ids = HashSet::new();
    for member_id in &decl.members {
        let member = if let Some(slot) = sources.get(member_id) {
            Some(Member::Source(slot.clone()))
        } else if defs.contains_key(member_id) {
            let node = build_node(member_id, sources, defs, built, limits)?;
            (node.composer.is_some()).then_some(Member::Composite(node))
        } else {
            None
        };
        let Some(member) = member else {
            if decl.lazy {
                continue;
            }
            return Err(MaptileError::Config(format!(
                "missing composite member {member_id} in {id}"
            )));
        };
        let descriptor = member.descriptor();
        if descriptor.info.format != TileFormat::Mvt
            || !descriptor.capabilities.strict_mvt
            || descriptor
                .tilejson
                .vector_layers
                .as_ref()
                .is_none_or(Vec::is_empty)
        {
            return Err(MaptileError::Config(format!(
                "member {member_id} lacks strict MVT declarations"
            )));
        }
        let children: Vec<_> = match &member {
            Member::Source(slot) => vec![slot.clone()],
            Member::Composite(node) => node.leaves.to_vec(),
        };
        for child in children {
            if !leaf_ids.insert(child.descriptor().id.clone()) {
                return Err(MaptileError::Config(format!(
                    "duplicate leaf {} in {id}",
                    child.descriptor().id
                )));
            }
            leaves.push(child);
            if leaves.len() > limits.max_leaves {
                return Err(MaptileError::Config(format!("too many leaves in {id}")));
            }
        }
        members.push(member);
    }
    let mut builder = MvtComposer::builder()
        .duplicate_layer(decl.duplicate_layer)
        .output_compression(Compression::None)
        .max_decoded_bytes(
            limits
                .max_decoded_bytes
                .min(transport::MAX_DECODED_TILE_BYTES),
        );
    let mut layers = Vec::<tilejson::VectorLayer>::new();
    let mut tj = tilejson::tilejson! { tiles: vec![] };
    tj.name = Some(format!("composite:{id}"));
    let mut caps = SourceCapabilities {
        filter: true,
        bbox: true,
        url_query: true,
        raster_style: !members.is_empty(),
        strict_mvt: true,
        consistent_export: true,
        ..Default::default()
    };
    let mut zoom = CacheZoomRange::default();
    let mut hash = blake3::Hasher::new();
    hash.update(
        format!(
            "composite-v1:{:?}:{:?}",
            decl.duplicate_layer, decl.failure_policy
        )
        .as_bytes(),
    );
    for (index, member) in members.iter().enumerate() {
        let d = member.descriptor();
        let own_layers = d.tilejson.vector_layers.as_ref().expect("checked above");
        builder = builder.add_source(
            MvtSource::new(d.id.to_string()).with_layers(own_layers.iter().map(|l| l.id.clone())),
        );
        for layer in own_layers {
            if let Some(existing) = layers.iter_mut().find(|existing| existing.id == layer.id) {
                for (field, description) in &layer.fields {
                    existing
                        .fields
                        .entry(field.clone())
                        .and_modify(|current| {
                            if current != description {
                                *current = "Varies by source".into();
                            }
                        })
                        .or_insert_with(|| description.clone());
                }
                existing.minzoom = existing.minzoom.zip(layer.minzoom).map(|(a, b)| a.min(b));
                existing.maxzoom = existing.maxzoom.zip(layer.maxzoom).map(|(a, b)| a.max(b));
            } else {
                layers.push(layer.clone());
            }
        }
        tj.bounds = if index == 0 {
            d.tilejson.bounds
        } else {
            tj.bounds.zip(d.tilejson.bounds).map(|(a, b)| {
                tilejson::Bounds::new(
                    a.left.min(b.left),
                    a.bottom.min(b.bottom),
                    a.right.max(b.right),
                    a.top.max(b.top),
                )
            })
        };
        tj.minzoom = match (tj.minzoom, d.tilejson.minzoom) {
            (None, z) if index == 0 => z,
            (Some(a), Some(b)) => Some(a.min(b)),
            _ => None,
        };
        tj.maxzoom = match (tj.maxzoom, d.tilejson.maxzoom) {
            (None, z) if index == 0 => z,
            (Some(a), Some(b)) => Some(a.max(b)),
            _ => None,
        };
        caps.filter &= d.capabilities.filter;
        caps.bbox &= d.capabilities.bbox;
        caps.url_query &= d.capabilities.url_query;
        caps.raster_style &= d.capabilities.raster_style;
        caps.consistent_export &= d.capabilities.consistent_export;
        zoom = zoom.intersection(d.cache_zoom);
        hash.update(&(d.id.len() as u64).to_le_bytes());
        hash.update(d.id.as_bytes());
        hash.update(d.revision.as_bytes());
    }
    if !layers.is_empty() {
        tj.vector_layers = Some(layers);
    }
    let composer = if members.is_empty() {
        None
    } else {
        Some(Arc::new(builder.build().map_err(|e| {
            MaptileError::Config(format!("composite {id}: {e}"))
        })?))
    };
    let descriptor = SourceDescriptor {
        id: id.clone(),
        kind: SourceKind::Composite,
        tilejson: tj,
        info: TileInfo {
            format: TileFormat::Mvt,
            encoding: TileEncoding::Uncompressed,
        },
        revision: ContentVersion::from_bytes(*hash.finalize().as_bytes()),
        cache_zoom: zoom,
        capabilities: caps,
    };
    let node = Arc::new(CompiledComposite {
        descriptor,
        leaves: leaves.into_boxed_slice(),
        members: members.into_boxed_slice(),
        composer,
        policy: decl.failure_policy,
    });
    built.insert(id.clone(), node.clone());
    Ok(node)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        control::TileDriver,
        service::TileOutcome,
        sources::mvt::{self, proto},
    };
    use geozero::mvt::Message;
    use tilejson::{TileJSON, VectorLayer};

    #[derive(Clone)]
    struct Fake {
        id: String,
        tj: TileJSON,
        data: Result<Bytes, MaptileError>,
    }
    #[async_trait::async_trait]
    impl TileDriver for Fake {
        fn descriptor(&self) -> SourceDescriptor {
            SourceDescriptor {
                id: Arc::from(self.id.as_str()),
                kind: SourceKind::Other,
                tilejson: self.tj.clone(),
                info: TileInfo {
                    format: TileFormat::Mvt,
                    encoding: TileEncoding::Uncompressed,
                },
                revision: ContentVersion::from_input(self.id.as_bytes()),
                cache_zoom: CacheZoomRange::default(),
                capabilities: SourceCapabilities {
                    strict_mvt: true,
                    ..Default::default()
                },
            }
        }
        async fn read(
            &self,
            _: TileCoord,
            _: Option<&TileQuery<'_>>,
        ) -> MaptileResult<Option<TileResponse>> {
            match &self.data {
                Ok(data) => Ok(Some(TileResponse {
                    data: data.clone(),
                    info: self.descriptor().info,
                    last_modified: None,
                })),
                Err(MaptileError::TileNotFound(..)) => Ok(None),
                Err(error) => Err(error.clone()),
            }
        }
    }
    fn tile(layers: &[(&str, u32)]) -> Bytes {
        let tile = proto::Tile {
            layers: layers
                .iter()
                .map(|(name, extent)| proto::Layer {
                    version: 2,
                    name: (*name).into(),
                    features: vec![],
                    keys: vec![],
                    values: vec![],
                    extent: Some(*extent),
                })
                .collect(),
        };
        let mut bytes = Vec::new();
        tile.encode(&mut bytes).unwrap();
        Bytes::from(bytes)
    }
    fn source(id: &str, declared: &[&str], data: Result<Bytes, MaptileError>) -> Arc<SourceSlot> {
        let mut tj = tilejson::tilejson! { tiles: vec![] };
        tj.vector_layers = Some(
            declared
                .iter()
                .map(|name| VectorLayer::new((*name).into(), Default::default()))
                .collect(),
        );
        let fake = Fake {
            id: id.into(),
            tj,
            data,
        };
        Arc::new(SourceSlot::new(Arc::new(fake)).unwrap())
    }
    fn decl(
        id: &str,
        members: &[&str],
        duplicate_layer: DuplicateLayer,
        policy: MemberFailurePolicy,
        lazy: bool,
    ) -> CompositeDeclaration {
        CompositeDeclaration {
            id: Arc::from(id),
            members: members.iter().map(|s| Arc::from(*s)).collect(),
            lazy,
            duplicate_layer,
            failure_policy: policy,
        }
    }
    fn coord() -> TileCoord {
        TileCoord { z: 0, x: 0, y: 0 }
    }
    fn nodes(
        sources: Vec<Arc<SourceSlot>>,
        declarations: &[CompositeDeclaration],
    ) -> MaptileResult<HashMap<Arc<str>, Arc<CompiledComposite>>> {
        compile_graph(
            &sources
                .into_iter()
                .map(|s| (s.descriptor().id.clone(), s))
                .collect(),
            declarations,
            CompositeLimits::default(),
        )
    }
    async fn read(node: &CompiledComposite) -> MaptileResult<TileOutcome> {
        node.read(coord(), None, &ReadBudget::new(CompositeLimits::default()))
            .await
    }
    fn data(outcome: TileOutcome) -> Bytes {
        match outcome {
            TileOutcome::Present(t) => t.data,
            _ => panic!("expected present"),
        }
    }

    #[test]
    fn rejects_cycle_missing_regular_duplicate_leaf_and_unknown_layers() {
        let a = source("a", &["roads"], Ok(tile(&[("roads", 1)])));
        assert!(
            nodes(
                vec![],
                &[
                    decl(
                        "x",
                        &["y"],
                        DuplicateLayer::Error,
                        MemberFailurePolicy::FailFast,
                        true
                    ),
                    decl(
                        "y",
                        &["x"],
                        DuplicateLayer::Error,
                        MemberFailurePolicy::FailFast,
                        true
                    )
                ]
            )
            .is_err()
        );
        assert!(
            nodes(
                vec![],
                &[decl(
                    "x",
                    &["missing"],
                    DuplicateLayer::Error,
                    MemberFailurePolicy::FailFast,
                    false
                )]
            )
            .is_err()
        );
        assert!(
            nodes(
                vec![a.clone()],
                &[decl(
                    "x",
                    &["a", "a"],
                    DuplicateLayer::KeepFirst,
                    MemberFailurePolicy::FailFast,
                    false
                )]
            )
            .is_err()
        );
        assert!(
            nodes(
                vec![source("unknown", &[], Ok(Bytes::new()))],
                &[decl(
                    "x",
                    &["unknown"],
                    DuplicateLayer::Error,
                    MemberFailurePolicy::FailFast,
                    false
                )]
            )
            .is_err()
        );
        let lazy = nodes(
            vec![],
            &[decl(
                "x",
                &["missing"],
                DuplicateLayer::Error,
                MemberFailurePolicy::FailFast,
                true,
            )],
        )
        .unwrap();
        assert!(lazy["x"].leaves().is_empty());
    }
    #[test]
    fn depth_limit_counts_declarations_and_rejects_deep_graph_before_building() {
        let mut declarations = Vec::new();
        for n in 0..65 {
            declarations.push(CompositeDeclaration {
                id: format!("node{n}").into(),
                members: vec![if n == 64 {
                    "leaf".into()
                } else {
                    format!("node{}", n + 1).into()
                }],
                lazy: true,
                duplicate_layer: DuplicateLayer::Error,
                failure_policy: MemberFailurePolicy::FailFast,
            });
        }
        assert!(nodes(vec![], &declarations).is_err());
        declarations.remove(0);
        assert!(nodes(vec![], &declarations).is_ok());
    }
    #[tokio::test]
    async fn conflicts_are_selected_from_actual_per_tile_layers() {
        let a = source("a", &["roads", "water"], Ok(tile(&[("roads", 11)])));
        let b = source(
            "b",
            &["roads", "water"],
            Ok(tile(&[("roads", 22), ("water", 33)])),
        );
        for (policy, extent) in [
            (DuplicateLayer::KeepFirst, 11),
            (DuplicateLayer::KeepLast, 22),
        ] {
            let graph = nodes(
                vec![a.clone(), b.clone()],
                &[decl(
                    "x",
                    &["a", "b"],
                    policy,
                    MemberFailurePolicy::FailFast,
                    false,
                )],
            )
            .unwrap();
            let bytes = data(read(&graph["x"]).await.unwrap());
            let result = mvt::decode(&bytes).unwrap();
            assert_eq!(result.layers.len(), 2);
            assert_eq!(
                result
                    .layers
                    .iter()
                    .find(|l| l.name == "roads")
                    .unwrap()
                    .extent,
                Some(extent)
            );
        }
    }
    #[tokio::test]
    async fn rejects_undeclared_actual_layer_even_when_error_policy_declares_no_conflict() {
        let a = source("a", &["roads"], Ok(tile(&[("roads", 1)])));
        let b = source("b", &["water"], Ok(tile(&[("roads", 2)])));
        let graph = nodes(
            vec![a, b],
            &[decl(
                "x",
                &["a", "b"],
                DuplicateLayer::Error,
                MemberFailurePolicy::FailFast,
                false,
            )],
        )
        .unwrap();
        assert!(matches!(
            read(&graph["x"]).await,
            Err(MaptileError::InvalidMetadata(_))
        ));
        let duplicate = source(
            "duplicate",
            &["roads"],
            Ok(tile(&[("roads", 1), ("roads", 2)])),
        );
        let graph = nodes(
            vec![duplicate],
            &[decl(
                "x",
                &["duplicate"],
                DuplicateLayer::KeepFirst,
                MemberFailurePolicy::FailFast,
                false,
            )],
        )
        .unwrap();
        assert!(matches!(
            read(&graph["x"]).await,
            Err(MaptileError::InvalidMetadata(_))
        ));
    }
    #[tokio::test]
    async fn empty_is_present_no_tile_is_absent_and_transient_skip_is_partial() {
        let empty = source("empty", &["roads"], Ok(Bytes::new()));
        let absent = source(
            "absent",
            &["water"],
            Err(MaptileError::TileNotFound(0, 0, 0)),
        );
        let unavailable = source(
            "fail",
            &["rivers"],
            Err(MaptileError::SourceUnavailable("fail".into())),
        );
        let graph = nodes(
            vec![empty, absent, unavailable],
            &[
                decl(
                    "zero",
                    &["empty", "absent"],
                    DuplicateLayer::Error,
                    MemberFailurePolicy::FailFast,
                    false,
                ),
                decl(
                    "partial",
                    &["zero", "fail"],
                    DuplicateLayer::Error,
                    MemberFailurePolicy::SkipTransient,
                    false,
                ),
            ],
        )
        .unwrap();
        assert!(data(read(&graph["zero"]).await.unwrap()).is_empty());
        match read(&graph["partial"]).await.unwrap() {
            TileOutcome::Partial {
                tile: Some(tile),
                failures,
            } => {
                assert!(tile.data.is_empty());
                assert_eq!(failures.len(), 1);
            }
            _ => panic!("expected partial"),
        }
    }
    #[tokio::test]
    async fn nested_policy_and_total_limit_are_preserved() {
        let fail = source(
            "fail",
            &["failed"],
            Err(MaptileError::SourceUnavailable("fail".into())),
        );
        let ok = source("ok", &["ok"], Ok(tile(&[("ok", 1)])));
        let graph = nodes(
            vec![fail, ok],
            &[
                decl(
                    "inner",
                    &["fail"],
                    DuplicateLayer::Error,
                    MemberFailurePolicy::FailFast,
                    false,
                ),
                decl(
                    "outer",
                    &["inner", "ok"],
                    DuplicateLayer::Error,
                    MemberFailurePolicy::SkipTransient,
                    false,
                ),
            ],
        )
        .unwrap();
        assert!(matches!(
            read(&graph["outer"]).await.unwrap(),
            TileOutcome::Partial { tile: Some(_), .. }
        ));
        let limits = CompositeLimits {
            max_decoded_bytes: 1,
            ..CompositeLimits::default()
        };
        assert!(matches!(
            graph["outer"]
                .read(coord(), None, &ReadBudget::new(limits))
                .await,
            Err(MaptileError::PayloadTooLarge { .. })
        ));
    }
    #[tokio::test]
    async fn all_skipped_is_an_error_and_invalid_input_is_never_skipped() {
        let a = source(
            "a",
            &["a"],
            Err(MaptileError::SourceUnavailable("a".into())),
        );
        let b = source(
            "b",
            &["b"],
            Err(MaptileError::SourceUnavailable("b".into())),
        );
        let graph = nodes(
            vec![a, b],
            &[decl(
                "x",
                &["a", "b"],
                DuplicateLayer::Error,
                MemberFailurePolicy::SkipTransient,
                false,
            )],
        )
        .unwrap();
        assert!(matches!(
            read(&graph["x"]).await,
            Err(MaptileError::SourceUnavailable(_))
        ));
        let invalid = source(
            "invalid",
            &["invalid"],
            Err(MaptileError::InvalidRequest("invalid".into())),
        );
        let graph = nodes(
            vec![invalid],
            &[decl(
                "x",
                &["invalid"],
                DuplicateLayer::Error,
                MemberFailurePolicy::SkipTransient,
                false,
            )],
        )
        .unwrap();
        assert!(matches!(
            read(&graph["x"]).await,
            Err(MaptileError::InvalidRequest(_))
        ));
    }
    #[tokio::test]
    async fn cumulative_decoded_budget_rejects_two_large_valid_members() {
        fn large(name: &str) -> Bytes {
            let tile = proto::Tile {
                layers: vec![proto::Layer {
                    version: 2,
                    name: name.into(),
                    features: vec![],
                    keys: vec!["a".repeat(9 * 1024 * 1024)],
                    values: vec![],
                    extent: Some(4096),
                }],
            };
            let mut bytes = Vec::new();
            tile.encode(&mut bytes).unwrap();
            Bytes::from(bytes)
        }
        let a = source("a", &["a"], Ok(large("a")));
        let b = source("b", &["b"], Ok(large("b")));
        let graph = nodes(
            vec![a, b],
            &[decl(
                "x",
                &["a", "b"],
                DuplicateLayer::Error,
                MemberFailurePolicy::FailFast,
                false,
            )],
        )
        .unwrap();
        let limits = CompositeLimits {
            max_compressed_bytes: 32 * 1024 * 1024,
            ..CompositeLimits::default()
        };
        assert!(matches!(
            graph["x"]
                .read(coord(), None, &ReadBudget::new(limits))
                .await,
            Err(MaptileError::PayloadTooLarge { .. })
        ));
    }
}
