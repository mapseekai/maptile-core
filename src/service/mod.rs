//! Public XYZ reads, ordered application layers and version-pinned export sessions.

pub(crate) mod engine;
mod export;

pub use crate::core::response::{MemberFailure, TileOutcome};
pub use export::{BatchPage, ExportCursor, ExportSession};

use std::sync::{Arc, OnceLock};

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use self::engine::{CatalogService, check_output};
use crate::control::{CatalogEntry, CatalogSnapshot, SourceDescriptor};
use crate::error::{MaptileError, MaptileResult};
use crate::sources::{
    RasterPointQuery, RasterPointResponse, TileCoord, TileQuery, TileResponse,
    raster_coverage::{CoverageQuery, CoverageResponse, MAX_COVERAGE_BYTES},
    validate_tile_request,
};

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
    checked: &'a OnceLock<TileResponse>,
}

impl TileService<'_> {
    pub async fn read(self) -> MaptileResult<TileOutcome> {
        match self.layers.split_first() {
            Some((layer, layers)) => {
                layer
                    .read(self.request, TileService { layers, ..self })
                    .await
            }
            None => {
                let result = self.engine.read_canonical(self.request).await?;
                if let Some(tile) = result.tile()
                    && tile.info.encoding.is_encoded()
                {
                    let _ = self.checked.set(tile.clone());
                }
                Ok(result)
            }
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
            request: ReadTarget {
                source: &entry.descriptor().id,
                ..request.target
            }
            .at(request.coord),
            snapshot,
            entry,
        };
        let read = async {
            if self.layers.is_empty() {
                return self.engine.read_canonical(&pinned).await;
            }
            let checked = OnceLock::new();
            let result = TileService {
                layers: &self.layers,
                engine: &self.engine,
                request: &pinned,
                checked: &checked,
            }
            .read()
            .await?;
            // Retain the checked allocation so its pointer cannot be reused by a rewrite.
            if !checked
                .get()
                .zip(result.tile())
                .is_some_and(|(before, after)| {
                    before.info.encoding == after.info.encoding
                        && std::ptr::eq(before.data.as_ref(), after.data.as_ref())
                })
            {
                check_output(&result).await?;
            }
            Ok(result)
        };
        bounded(request.target, entry, read).await
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
