//! Public XYZ reads, ordered application layers and version-pinned export sessions.

use crate::cache::{ContentVersion, StyleHash, TileCacheKey, hashing::canonical_render_hash};
use crate::control::{CatalogEntry, CatalogService, CatalogSnapshot, SourceDescriptor, SourceId};
use crate::error::{MaptileError, MaptileResult};
use crate::sources::{
    MAX_TILE_BATCH_BYTES, MAX_TILE_BATCH_COORDS, MosaicPixelSelection, RasterPointQuery,
    RasterPointResponse, TileCoord, TileEncoding, TileQuery, TileResponse,
    effective_pixel_selection,
    raster_coverage::{CoverageQuery, CoverageResponse, MAX_COVERAGE_BYTES},
    tile_math::validate_tile_request,
};
use crate::transport::MAX_DECODED_TILE_BYTES;
use futures_util::{
    FutureExt,
    future::{BoxFuture, Shared},
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct MemberFailure {
    pub source: SourceId,
    pub error: MaptileError,
}

#[derive(Debug, Clone)]
pub enum TileOutcome {
    NoTile,
    Present(TileResponse),
    Partial {
        tile: Option<TileResponse>,
        failures: Vec<MemberFailure>,
    },
}

impl TileOutcome {
    pub fn tile(&self) -> Option<&TileResponse> {
        match self {
            Self::NoTile => None,
            Self::Present(tile) => Some(tile),
            Self::Partial { tile, .. } => tile.as_ref(),
        }
    }
    pub fn is_partial(&self) -> bool {
        matches!(self, Self::Partial { .. })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ReadTarget<'a> {
    pub source: &'a str,
    /// Opaque visibility scope chosen by the caller before admission.
    pub scope: &'a str,
    pub query: Option<&'a TileQuery<'a>>,
    pub deadline: Option<Instant>,
    pub cancellation: Option<&'a CancellationToken>,
}

impl<'a> ReadTarget<'a> {
    pub fn new(source: &'a str, scope: &'a str) -> Self {
        Self {
            source,
            scope,
            query: None,
            deadline: None,
            cancellation: None,
        }
    }
    pub fn at(self, coord: TileCoord) -> TileRequest<'a> {
        TileRequest {
            target: self,
            coord,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TileRequest<'a> {
    pub target: ReadTarget<'a>,
    pub coord: TileCoord,
}

pub struct ValidatedTileRequest<'a> {
    request: TileRequest<'a>,
    snapshot: &'a CatalogSnapshot,
    entry: &'a Arc<CatalogEntry>,
}

impl<'a> ValidatedTileRequest<'a> {
    pub fn request(&self) -> &TileRequest<'a> {
        &self.request
    }
    pub fn descriptor(&self) -> &SourceDescriptor {
        self.entry.descriptor()
    }
    pub fn generation(&self) -> u64 {
        self.snapshot.generation()
    }
}

#[async_trait::async_trait]
pub trait TileLayer: Send + Sync + 'static {
    async fn read(
        &self,
        request: &ValidatedTileRequest<'_>,
        next: TileService<'_>,
    ) -> MaptileResult<TileOutcome>;
}

/// One use of the remaining layers with the same admitted request.
pub struct TileService<'a> {
    layers: &'a [Arc<dyn TileLayer>],
    engine: &'a CatalogService,
    request: &'a ValidatedTileRequest<'a>,
}

impl TileService<'_> {
    pub async fn read(self) -> MaptileResult<TileOutcome> {
        match self.layers.split_first() {
            Some((layer, layers)) => {
                layer
                    .read(self.request, TileService { layers, ..self })
                    .await
            }
            None => self.engine.read_canonical(self.request).await,
        }
    }
}

#[derive(Clone)]
pub struct TileStack {
    engine: Arc<CatalogService>,
    layers: Arc<[Arc<dyn TileLayer>]>,
}

pub struct TileStackBuilder {
    engine: Arc<CatalogService>,
    layers: Vec<Arc<dyn TileLayer>>,
}

impl TileStackBuilder {
    pub fn layer(mut self, layer: impl TileLayer) -> Self {
        self.layers.push(Arc::new(layer));
        self
    }
    pub fn build(self) -> TileStack {
        TileStack {
            engine: self.engine,
            layers: self.layers.into(),
        }
    }
}

impl TileStack {
    pub fn builder(engine: Arc<CatalogService>) -> TileStackBuilder {
        TileStackBuilder {
            engine,
            layers: Vec::new(),
        }
    }

    pub async fn read(&self, request: TileRequest<'_>) -> MaptileResult<TileOutcome> {
        let snapshot = self.engine.catalog().snapshot();
        let entry = snapshot.entry(request.target.source, self.engine.limits())?;
        self.dispatch(&snapshot, &entry, request).await
    }

    async fn dispatch(
        &self,
        snapshot: &CatalogSnapshot,
        entry: &Arc<CatalogEntry>,
        request: TileRequest<'_>,
    ) -> MaptileResult<TileOutcome> {
        validate_tile_request(&[request.coord], request.target.query)?;
        entry.descriptor().validate_query(request.target.query)?;
        let pinned = ValidatedTileRequest {
            request,
            snapshot,
            entry,
        };
        let read = async {
            let result = TileService {
                layers: &self.layers,
                engine: &self.engine,
                request: &pinned,
            }
            .read()
            .await?;
            if !self.layers.is_empty() {
                check_output(&result).await?;
            }
            Ok(result)
        };
        bounded(request.target, entry, read).await
    }

    pub async fn read_batch(
        &self,
        target: ReadTarget<'_>,
        coords: &[TileCoord],
    ) -> MaptileResult<BatchPage> {
        self.session(target, false)?.read_page(coords).await
    }

    /// Raw raster samples and coverage use their own query shapes and bypass tile layers and cache.
    pub async fn raster_point(
        &self,
        target: ReadTarget<'_>,
        query: &RasterPointQuery,
    ) -> MaptileResult<RasterPointResponse> {
        if !query.x.is_finite() || !query.y.is_finite() || query.bidx.contains(&0) {
            return Err(MaptileError::InvalidRequest(
                "invalid raster point or band index".into(),
            ));
        }
        let entry = self.raster_entry(target)?;
        if let CatalogEntry::Source(slot) = entry.as_ref()
            && slot.descriptor().capabilities.raster_point
        {
            return bounded(target, &entry, slot.point(query)).await;
        }
        Err(MaptileError::Unsupported("raster point query".into()))
    }

    pub async fn raster_coverage(
        &self,
        target: ReadTarget<'_>,
        query: Option<&CoverageQuery>,
    ) -> MaptileResult<CoverageResponse> {
        if let Some(query) = query {
            query.validate()?;
        }
        let entry = self.raster_entry(target)?;
        if let CatalogEntry::Source(slot) = entry.as_ref()
            && slot.descriptor().capabilities.raster_coverage
        {
            let coverage = bounded(target, &entry, slot.coverage(query)).await?;
            if coverage.data.len() > MAX_COVERAGE_BYTES {
                return Err(MaptileError::PayloadTooLarge {
                    limit: MAX_COVERAGE_BYTES,
                });
            }
            return Ok(coverage);
        }
        Err(MaptileError::Unsupported("raster coverage".into()))
    }

    fn raster_entry(&self, target: ReadTarget<'_>) -> MaptileResult<Arc<CatalogEntry>> {
        if target.query.is_some() {
            return Err(MaptileError::Unsupported(
                "tile options on a raw raster query".into(),
            ));
        }
        self.engine
            .catalog()
            .snapshot()
            .entry(target.source, self.engine.limits())
    }

    /// Pin the entry and every transitive source across pages. Mutable drivers must opt in.
    pub fn begin_export<'a>(&self, target: ReadTarget<'a>) -> MaptileResult<ExportSession<'a>> {
        self.session(target, true)
    }

    /// Resume a caller-owned, stable coordinate sequence after a process or request boundary.
    /// The caller authenticates every request and retains the same layer configuration and order.
    pub async fn read_export_page(
        &self,
        target: ReadTarget<'_>,
        coords: &[TileCoord],
        cursor: Option<&ExportCursor>,
    ) -> MaptileResult<(BatchPage, Option<ExportCursor>)> {
        let session = self.begin_export(target)?;
        let descriptor = session.entry.descriptor();
        let (_, render) = render_identity(descriptor, target.query);
        let mut hash = blake3::Hasher::new();
        hash.update(descriptor.revision.as_bytes());
        hash.update(render.as_bytes());
        serde_json::to_writer(
            &mut hash,
            &(
                target.source,
                target.scope,
                target.query.and_then(|q| q.filter),
                target.query.and_then(|q| q.url_query),
                target.query.and_then(|q| q.bbox),
            ),
        )
        .map_err(MaptileError::backend)?;
        let revision = *hash.finalize().as_bytes();
        if cursor.is_some_and(|c| c.revision != revision) {
            return Err(MaptileError::VersionChanged(target.source.into()));
        }
        let start = cursor.map_or(0, |c| c.position);
        let rest = coords.get(start..).ok_or_else(|| {
            MaptileError::InvalidRequest("export cursor exceeds coordinate sequence".into())
        })?;
        let page = session
            .read_page(&rest[..rest.len().min(MAX_TILE_BATCH_COORDS)])
            .await?;
        let position = start + page.consumed;
        Ok((
            page,
            (position < coords.len()).then_some(ExportCursor { revision, position }),
        ))
    }

    fn session<'a>(
        &self,
        target: ReadTarget<'a>,
        consistent: bool,
    ) -> MaptileResult<ExportSession<'a>> {
        validate_tile_request(&[], target.query)?;
        let snapshot = self.engine.catalog().snapshot();
        let entry = snapshot.entry(target.source, self.engine.limits())?;
        let descriptor = entry.descriptor();
        descriptor.validate_query(target.query)?;
        if consistent && !descriptor.capabilities.consistent_export {
            return Err(MaptileError::Unsupported(format!(
                "{} cannot pin its data for export",
                descriptor.id
            )));
        }
        Ok(ExportSession {
            stack: self.clone(),
            target,
            snapshot,
            entry,
        })
    }
}

#[derive(Clone, Debug)]
pub struct BatchPage {
    pub tiles: Vec<TileOutcome>,
    pub consumed: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ExportCursor {
    revision: [u8; 32],
    position: usize,
}

impl ExportCursor {
    pub fn position(&self) -> usize {
        self.position
    }
}

pub struct ExportSession<'a> {
    stack: TileStack,
    target: ReadTarget<'a>,
    snapshot: Arc<CatalogSnapshot>,
    entry: Arc<CatalogEntry>,
}

impl ExportSession<'_> {
    pub fn revision(&self) -> ContentVersion {
        self.entry.descriptor().revision
    }
    pub fn generation(&self) -> u64 {
        self.snapshot.generation()
    }
    pub async fn read_page(&self, coords: &[TileCoord]) -> MaptileResult<BatchPage> {
        bounded(self.target, &self.entry, self.read_page_inner(coords)).await
    }

    async fn read_page_inner(&self, coords: &[TileCoord]) -> MaptileResult<BatchPage> {
        validate_tile_request(coords, self.target.query)?;
        if self.stack.layers.is_empty()
            && let CatalogEntry::Source(slot) = self.entry.as_ref()
            && slot.descriptor().capabilities.batch
            && let Some(rows) = slot.read_batch(coords, self.target.query).await?
        {
            if rows.len() > coords.len() || (!coords.is_empty() && rows.is_empty()) {
                return Err(MaptileError::InvalidMetadata(
                    "invalid accelerated batch length".into(),
                ));
            }
            let mut tiles = Vec::with_capacity(rows.len());
            let mut bytes = 0;
            for row in rows {
                let outcome = row.map_or(TileOutcome::NoTile, TileOutcome::Present);
                check_output(&outcome).await?;
                bytes += outcome.tile().map_or(0, |t| t.data.len());
                tiles.push(outcome);
                if bytes >= MAX_TILE_BATCH_BYTES {
                    break;
                }
            }
            return Ok(BatchPage {
                consumed: tiles.len(),
                tiles,
            });
        }
        let mut tiles = Vec::with_capacity(coords.len());
        let mut bytes = 0;
        for &coord in coords {
            let outcome = self
                .stack
                .dispatch(&self.snapshot, &self.entry, self.target.at(coord))
                .await?;
            bytes += outcome.tile().map_or(0, |t| t.data.len());
            tiles.push(outcome);
            if bytes >= MAX_TILE_BATCH_BYTES {
                break;
            }
        }
        Ok(BatchPage {
            consumed: tiles.len(),
            tiles,
        })
    }
}

async fn bounded<T>(
    target: ReadTarget<'_>,
    entry: &CatalogEntry,
    read: impl Future<Output = MaptileResult<T>>,
) -> MaptileResult<T> {
    tokio::select! {
        biased;
        () = async { match target.cancellation {
            Some(token) => token.cancelled().await, None => std::future::pending().await,
        } } => Err(MaptileError::Cancelled),
        () = entry.revoked() => Err(MaptileError::Cancelled),
        () = async { match target.deadline {
            Some(deadline) if deadline <= Instant::now() => (),
            Some(deadline) => tokio::time::sleep_until(deadline).await, None => std::future::pending().await,
        } } => Err(MaptileError::Deadline),
        result = read => {
            if target.cancellation.is_some_and(CancellationToken::is_cancelled) ||
                entry.leaves().iter().any(|slot| slot.is_revoked()) {
                Err(MaptileError::Cancelled)
            } else if target.deadline.is_some_and(|deadline| deadline <= Instant::now()) {
                Err(MaptileError::Deadline)
            } else { result }
        },
    }
}

async fn check_output(outcome: &TileOutcome) -> MaptileResult<()> {
    let Some(tile) = outcome.tile() else {
        return Ok(());
    };
    if tile.data.len() > MAX_DECODED_TILE_BYTES {
        return Err(MaptileError::PayloadTooLarge {
            limit: MAX_DECODED_TILE_BYTES,
        });
    }
    if !matches!(
        tile.info.encoding,
        TileEncoding::Internal | TileEncoding::Uncompressed
    ) {
        crate::transport::decode_async(tile.data.clone(), tile.info.encoding).await?;
    }
    Ok(())
}

#[derive(Clone, Eq, Hash, PartialEq)]
struct FlightKey {
    tile: TileCacheKey,
    generation: u64,
}
type FlightTable = Mutex<HashMap<FlightKey, Weak<Flight>>>;
#[derive(Default)]
pub(crate) struct Flights(Arc<FlightTable>);
struct Flight {
    table: Weak<FlightTable>,
    key: FlightKey,
    result: Shared<BoxFuture<'static, MaptileResult<TileOutcome>>>,
}

impl Drop for Flight {
    fn drop(&mut self) {
        if let Some(table) = self.table.upgrade() {
            let mut table = table.lock().unwrap();
            if table
                .get(&self.key)
                .is_some_and(|weak| std::ptr::eq(weak.as_ptr(), self))
            {
                table.remove(&self.key);
            }
        }
    }
}

impl Flights {
    fn join(
        &self,
        key: FlightKey,
        fill: impl FnOnce() -> BoxFuture<'static, MaptileResult<TileOutcome>>,
    ) -> Arc<Flight> {
        let mut table = self.0.lock().unwrap();
        if let Some(active) = table.get(&key).and_then(Weak::upgrade) {
            return active;
        }
        let flight = Arc::new(Flight {
            table: Arc::downgrade(&self.0),
            key: key.clone(),
            result: fill().shared(),
        });
        table.insert(key, Arc::downgrade(&flight));
        flight
    }
}

fn render_identity(
    descriptor: &SourceDescriptor,
    query: Option<&TileQuery<'_>>,
) -> (Option<MosaicPixelSelection>, StyleHash) {
    let style = query.and_then(|q| q.style);
    let selection = descriptor
        .capabilities
        .pixel_selection
        .map(|s| effective_pixel_selection(query, s));
    let (size, format) = style.map_or((256, 0), |s| {
        use crate::raster::style::RasterOutputFormat::*;
        (
            s.tile_size,
            match s.format {
                Png => 0,
                Webp => 1,
                Jpeg => 2,
            },
        )
    });
    let render = canonical_render_hash(style, selection, size, 1, format);
    let style_hash = match query.and_then(|q| q.style_hash) {
        None => render,
        Some(hash) => {
            let mut h = blake3::Hasher::new();
            h.update(hash.as_bytes());
            h.update(render.as_bytes());
            StyleHash::from_bytes(*h.finalize().as_bytes())
        }
    };
    (selection, style_hash)
}

impl CatalogService {
    async fn read_canonical(
        &self,
        pinned: &ValidatedTileRequest<'_>,
    ) -> MaptileResult<TileOutcome> {
        let request = pinned.request;
        let query = request.target.query;
        let descriptor = pinned.descriptor();
        if !descriptor.cache_zoom.contains(request.coord.z)
            || query.is_some_and(TileQuery::bypasses_cache)
        {
            let result = pinned
                .entry
                .read(request.coord, query, self.limits())
                .await?;
            check_output(&result).await?;
            return Ok(result);
        }
        let style = query.and_then(|q| q.style);
        let (selection, style_hash) = render_identity(descriptor, query);
        let mut identity = blake3::Hasher::new();
        identity.update(&crate::cache::hashing::CACHE_SCHEMA_VERSION.to_le_bytes());
        identity.update(descriptor.revision.as_bytes());
        identity.update(request.target.scope.as_bytes());
        let coord = request.coord;
        let key = TileCacheKey::new(
            descriptor.id.clone(),
            ContentVersion::from_bytes(*identity.finalize().as_bytes()),
            coord.z,
            coord.x,
            coord.y,
            style_hash,
        );
        if let Some(tile) = self.cache.get(&key).await {
            return Ok(TileOutcome::Present(tile));
        }

        let has_query = query.is_some();
        let requested_hash = query.and_then(|q| q.style_hash);
        let flight = self.flights.join(
            FlightKey {
                tile: key.clone(),
                generation: pinned.generation(),
            },
            || {
                let cache = self.cache.clone();
                let entry = pinned.entry.clone();
                let limits = self.limits();
                let style = style.cloned();
                async move {
                    if let Some(tile) = cache.get(&key).await {
                        return Ok(TileOutcome::Present(tile));
                    }
                    let query = TileQuery {
                        style: style.as_ref(),
                        style_hash: requested_hash,
                        pixel_selection: selection,
                        ..Default::default()
                    };
                    let result = entry
                        .read(
                            coord,
                            (has_query || selection.is_some()).then_some(&query),
                            limits,
                        )
                        .await?;
                    check_output(&result).await?;
                    if let TileOutcome::Present(tile) = &result {
                        cache.insert(key, tile.clone()).await;
                    }
                    Ok(result)
                }
                .boxed()
            },
        );
        flight.result.clone().await
    }
}
