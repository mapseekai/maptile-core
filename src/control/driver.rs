use crate::cache::{CacheZoomRange, ContentVersion};
use crate::core::SourceId;
use crate::error::{MaptileError, MaptileResult};
use crate::sources::{
    MosaicPixelSelection, RasterPointQuery, RasterPointResponse, SourceKind, TileCoord, TileInfo,
    TileQuery, TileResponse, TileSource,
    raster_coverage::{CoverageQuery, CoverageResponse},
};

/// Run a synchronous source constructor on a bounded blocking worker.
/// During [`SourceCatalog::reconcile`](super::SourceCatalog::reconcile), shutdown waits for the worker and for
/// disposal of a result abandoned by its caller. A constructor that starts
/// additional background work must retain it until the factory or driver drains.
///
/// ```no_run
/// use maptile_core::control::{DiscoveredDataset, Inventory, SourceCatalog, TileDriver, open_blocking};
/// use maptile_core::error::MaptileResult;
/// use std::sync::Arc;
/// # fn open_archive(_path: String) -> MaptileResult<Arc<dyn TileDriver>> { unimplemented!() }
/// # async fn refresh(catalog: &SourceCatalog, generation: u64, items: &[DiscoveredDataset]) -> MaptileResult<u64> {
/// catalog.reconcile(generation, "files", Inventory::Complete(items), |item| {
///     let path = item.dataset.to_string();
///     open_blocking(move || open_archive(path))
/// }).await
/// # }
/// ```
pub async fn open_blocking<T: Send + 'static>(
    open: impl FnOnce() -> MaptileResult<T> + Send + 'static,
) -> MaptileResult<T> {
    crate::transport::run_blocking(open).await
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SourceCapabilities {
    pub filter: bool,
    pub bbox: bool,
    pub url_query: bool,
    /// The driver applies per-request Raster Style. Defaults to `false`.
    pub raster_style: bool,
    pub pixel_selection: Option<MosaicPixelSelection>,
    pub strict_mvt: bool,
    pub consistent_export: bool,
    pub batch: bool,
    pub raster_point: bool,
    pub raster_coverage: bool,
}

/// Every entry uses the Web Mercator XYZ grid. TileJSON contains source metadata only.
#[derive(Clone, Debug)]
pub struct SourceDescriptor {
    pub id: SourceId,
    pub kind: SourceKind,
    pub tilejson: tilejson::TileJSON,
    pub info: TileInfo,
    pub revision: ContentVersion,
    pub cache_zoom: CacheZoomRange,
    pub capabilities: SourceCapabilities,
}

impl SourceDescriptor {
    pub fn new(id: impl Into<SourceId>, info: TileInfo, revision: ContentVersion) -> Self {
        Self {
            id: id.into(),
            kind: SourceKind::Other,
            tilejson: tilejson::tilejson! { tiles: vec![] },
            info,
            revision,
            cache_zoom: CacheZoomRange::default(),
            capabilities: SourceCapabilities::default(),
        }
    }

    pub(crate) fn validate_query(&self, query: Option<&TileQuery<'_>>) -> MaptileResult<()> {
        self.capabilities.validate_query(&self.id, query)
    }
}

impl SourceCapabilities {
    pub(crate) fn from_source<S: TileSource + ?Sized>(source: &S) -> Self {
        Self {
            filter: source.supports_filter(),
            bbox: source.supports_bbox(),
            url_query: source.supports_url_query(),
            raster_style: source.supports_raster_style(),
            pixel_selection: source.mosaic_pixel_selection(),
            strict_mvt: source.strict_mvt(),
            consistent_export: source.consistent_export(),
            batch: source.native_batch(),
            raster_point: source.supports_raster_point(),
            raster_coverage: source.supports_raster_coverage(),
        }
    }
    pub(crate) fn validate_query(
        self,
        id: &str,
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<()> {
        let Some(query) = query else { return Ok(()) };
        let c = self;
        let missing = if query.filter.is_some() && !c.filter {
            "CQL2"
        } else if query.bbox.is_some() && !c.bbox {
            "bbox"
        } else if query.url_query.is_some() && !c.url_query {
            "custom parameters"
        } else if query.has_raster_style() && !c.raster_style {
            "raster style"
        } else if query.pixel_selection.is_some() && c.pixel_selection.is_none() {
            "pixel selection"
        } else {
            return Ok(());
        };
        Err(MaptileError::Unsupported(format!(
            "{id} cannot apply {missing}"
        )))
    }
}

/// A third-party driver returns None for an absent coordinate and Some for a present tile,
/// including a zero-byte vector tile. The opened revision must match its descriptor.
#[async_trait::async_trait]
pub trait TileDriver: Send + Sync + 'static {
    fn descriptor(&self) -> SourceDescriptor;
    async fn read(
        &self,
        coord: TileCoord,
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<Option<TileResponse>>;
    /// Optional accelerated prefix with one position per input coordinate.
    async fn read_batch(
        &self,
        _coords: &[TileCoord],
        _query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<Option<Vec<Option<TileResponse>>>> {
        Ok(None)
    }
    async fn point(&self, _query: &RasterPointQuery) -> MaptileResult<RasterPointResponse> {
        Err(MaptileError::Unsupported("raster point query".into()))
    }
    async fn coverage(&self, _query: Option<&CoverageQuery>) -> MaptileResult<CoverageResponse> {
        Err(MaptileError::Unsupported("raster coverage".into()))
    }
    /// Signal shutdown synchronously. Implementations may receive this signal more than once.
    fn stop_admission(&self) {}
    /// Wait for source-owned work, including detached backend operations, to release its resources.
    async fn drain(&self) -> MaptileResult<()> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl<S: TileSource + ?Sized> TileDriver for S {
    fn descriptor(&self) -> SourceDescriptor {
        SourceDescriptor {
            id: self.id().into(),
            kind: self.source_kind(),
            tilejson: self.tilejson().clone(),
            info: self.tile_info(),
            revision: self.content_version(),
            cache_zoom: self.cache_zoom(),
            capabilities: SourceCapabilities::from_source(self),
        }
    }
    async fn read(
        &self,
        coord: TileCoord,
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<Option<TileResponse>> {
        match self.get_tile(coord, query).await {
            Ok(tile) => Ok(Some(tile)),
            Err(MaptileError::TileNotFound(..)) => Ok(None),
            Err(error) => Err(error),
        }
    }
    fn stop_admission(&self) {
        TileSource::start_close(self);
    }
    async fn point(&self, query: &RasterPointQuery) -> MaptileResult<RasterPointResponse> {
        self.query_raster_point(query).await
    }
    async fn coverage(&self, query: Option<&CoverageQuery>) -> MaptileResult<CoverageResponse> {
        self.raster_coverage(query).await
    }
    async fn read_batch(
        &self,
        coords: &[TileCoord],
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<Option<Vec<Option<TileResponse>>>> {
        if !self.native_batch() {
            return Ok(None);
        }
        self.read_tile_batch_slots(coords, query).await.map(Some)
    }
    async fn drain(&self) -> MaptileResult<()> {
        TileSource::close(self).await
    }
}
