//! Tile source contract and default per-source operations.
use super::{
    MosaicPixelSelection, RasterPointQuery, RasterPointResponse, TileCoord, TileInfo, TileQuery,
    TileResponse, archive_inspection, raster_coverage, request::read_tile_batch_sequential,
};
use crate::cache::CacheZoomRange;
use crate::error::MaptileResult;

/// Closed source capability used by application routing.
///
/// Sources are fail-closed by default. Only archive implementations that
/// explicitly identify themselves as MBTiles or PMTiles may serve stored
/// native tile bytes through the native-archive request path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceKind {
    NativeArchive(archive_inspection::ArchiveSourceType),
    PostgresTable,
    PostgresFunction,
    GeoParquet,
    FlatGeobuf,
    ParquetArchive,
    Cog,
    Mosaic,
    Stac,
    Composite,
    Other,
}

#[async_trait::async_trait]
pub trait TileSource: Send + Sync + 'static {
    fn id(&self) -> &str;

    fn source_kind(&self) -> SourceKind {
        SourceKind::Other
    }

    /// Revision of the content and metadata served by this opened source.
    /// Store a durable content fingerprint or allocate a
    /// [generation](crate::cache::ContentVersion::new_generation) when opening
    /// the source. Clones of the same handle must report the same revision.
    fn content_version(&self) -> crate::cache::ContentVersion;

    fn tilejson(&self) -> &tilejson::TileJSON;
    fn tile_info(&self) -> TileInfo;

    /// Stop accepting new work and begin releasing source-owned resources.
    /// Implementations without a persistent session need no special handling.
    fn start_close(&self) {}

    /// Wait until source-owned operations and connections have been released.
    async fn close(&self) -> MaptileResult<()> {
        self.start_close();
        Ok(())
    }

    async fn get_tile(
        &self,
        xyz: TileCoord,
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<TileResponse>;

    /// Read an ordered prefix of decoded tiles directly from the source.
    ///
    /// Empty tiles retain their position. A nonempty request returns at least
    /// one tile; resume at the returned length when the byte budget truncates it.
    /// Memory is bounded by the batch budget plus one decoded tile.
    async fn read_tile_batch(
        &self,
        coords: &[TileCoord],
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<Vec<TileResponse>> {
        read_tile_batch_sequential(self, coords, query).await
    }

    /// Accelerated drivers with missing rows preserve them as empty slots.
    async fn read_tile_batch_slots(
        &self,
        coords: &[TileCoord],
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<Vec<Option<TileResponse>>> {
        Ok(self
            .read_tile_batch(coords, query)
            .await?
            .into_iter()
            .map(Some)
            .collect())
    }

    fn supports_raster_coverage(&self) -> bool {
        false
    }

    /// Export native raster coverage, or return `InvalidRequest` if unsupported.
    async fn raster_coverage(
        &self,
        _query: Option<&raster_coverage::CoverageQuery>,
    ) -> MaptileResult<raster_coverage::CoverageResponse> {
        Err(crate::error::MaptileError::InvalidRequest(
            "source does not support native coverage export".into(),
        ))
    }

    fn supports_raster_point(&self) -> bool {
        false
    }

    /// Query a raw raster pixel value at a coordinate. Vector and non-raster
    /// sources return `InvalidRequest` from the default unsupported implementation.
    async fn query_raster_point(
        &self,
        _query: &RasterPointQuery,
    ) -> MaptileResult<RasterPointResponse> {
        Err(crate::error::MaptileError::InvalidRequest(format!(
            "source '{}' does not support raster point queries",
            self.id()
        )))
    }

    /// Whether the source consumes `TileQuery.url_query`.
    fn supports_url_query(&self) -> bool {
        false
    }

    /// Whether the source consumes `TileQuery.filter` (CQL2).
    fn supports_filter(&self) -> bool {
        false
    }

    /// Whether the source applies a per-request [`TileQuery::style`].
    fn supports_raster_style(&self) -> bool {
        false
    }

    fn supports_bbox(&self) -> bool {
        false
    }

    /// The source declares every MVT layer name. Composition checks each actual tile.
    fn strict_mvt(&self) -> bool {
        false
    }

    /// All reads use the opened revision or report a version change.
    fn consistent_export(&self) -> bool {
        matches!(self.source_kind(), SourceKind::NativeArchive(_))
    }

    fn native_batch(&self) -> bool {
        false
    }

    /// Persisted mosaic pixel-selection fallback. Non-mosaic sources return
    /// `None`, which also marks request-level mosaic selection unsupported.
    fn mosaic_pixel_selection(&self) -> Option<MosaicPixelSelection> {
        None
    }

    /// Zoom-level bounds for tile caching. Returned by every source
    /// to tell [`crate::control::CatalogService`] which zoom
    /// levels are worth caching for this source. The default returns
    /// [`CacheZoomRange::default()`] (cache every zoom).
    ///
    /// Override when a source has a structural reason to skip some
    /// zooms — e.g. dynamic SQL sources where deep zooms are too
    /// expensive to cache, or sources that already have an external
    /// CDN in front of them.
    fn cache_zoom(&self) -> CacheZoomRange {
        CacheZoomRange::default()
    }
}
