//! Tile source abstractions and registry.

pub mod archive_inspection;
pub mod composite;
pub mod lazy_composite;
pub mod manager;
pub mod mbtiles;
pub mod mvt;
pub mod pmtiles;
pub mod postgres;
pub mod raster_coverage;
pub mod tile_math;

pub use composite::{CompositeChild, CompositeSource};
pub use lazy_composite::LazyCompositeSource;

#[cfg(feature = "raster")]
pub mod dataset_cache;

#[cfg(feature = "agtiff")]
mod agtiff_cog;
#[cfg(feature = "agtiff")]
pub use agtiff_cog::{AsyncCogSource, AsyncCogSourceConfig, CogObjectSource};

#[cfg(all(feature = "agtiff", feature = "raster"))]
pub mod mosaic_source;
#[cfg(all(feature = "agtiff", feature = "raster"))]
pub use mosaic_source::{MosaicMember, MosaicSource, MosaicSourceConfig, pixel_selection_from_str};

/// The process-shared decoded-block cache type. Re-exported so the server
/// can build one at startup and inject it into both `AsyncCogSourceConfig`
/// and `MosaicSourceConfig`, bounding every COG reader (standalone or inside
/// a mosaic) to one decode-memory budget (async-geotiff-rs-note §11/§14).
#[cfg(feature = "agtiff")]
pub use async_geotiff::io::BlockCache;

/// Build a fresh process-shared [`BlockCache`] with the library default
/// capacity. Hold the returned `Arc` for the lifetime of the process and
/// clone it into every source config that accepts one.
#[cfg(feature = "agtiff")]
#[must_use]
pub fn new_shared_block_cache() -> std::sync::Arc<BlockCache> {
    std::sync::Arc::new(BlockCache::new(async_geotiff::io::CacheConfig::default()))
}

#[cfg(feature = "duckdb")]
pub mod duckdb;
#[cfg(feature = "duckdb")]
pub use self::duckdb::{DuckDbSource, DuckDbSourceConfig};

#[cfg(feature = "geoparquet")]
pub mod geoparquet;
#[cfg(feature = "geoparquet")]
mod geoparquet_geometry;
#[cfg(feature = "geoparquet")]
mod geoparquet_remote;
#[cfg(feature = "geoparquet")]
pub use geoparquet::{GeoParquetColumns, GeoParquetSource, GeoParquetSourceConfig};

#[cfg(feature = "local-vector")]
pub mod local_vector;
#[cfg(feature = "local-vector")]
pub use local_vector::{
    FlatGeobufSource, FlatGeobufSourceConfig, ImportedGeoParquet, VectorImportFormat,
    VectorImportOptions, import_to_geoparquet,
};

#[cfg(feature = "stac")]
pub mod stac;
#[cfg(feature = "stac")]
pub use stac::{
    StacAsset, StacSource, StacSourceConfig, build_search_url, extract_assets_from_item_collection,
    is_cog_mime_type, tile_to_wgs84_bbox,
};

use crate::cache::CacheZoomRange;
use crate::error::MaptileResult;
use crate::filter::{Cql2Expr, UrlQuery};

pub use martin_tile_utils::{Encoding as TileEncoding, Format as TileFormat, TileCoord, TileInfo};

pub const MAX_TILE_BATCH_COORDS: usize = 128;
pub const MAX_TILE_BATCH_BYTES: usize = 8 * 1024 * 1024;

/// Closed source capability used by application routing.
///
/// Sources are fail-closed by default. Only archive implementations that
/// explicitly identify themselves as MBTiles or PMTiles may serve stored
/// native tile bytes through the native-archive request path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceKind {
    NativeArchive(archive_inspection::ArchiveSourceType),
    Other,
}

/// Parse a comma-separated source id list (Martin-style composite URL
/// syntax: `/foo,bar,baz/{z}/{x}/{y}.pbf`). Returns the component ids
/// when at least one comma is present, or `None` for a single id (so
/// callers can fast-path the non-composite case).
///
/// Empty components and surrounding whitespace are stripped; if the
/// result has fewer than 2 components, `None` is returned. Pair with
/// [`crate::sources::manager::SourceManager::register_composite_from_ids`]
/// to build the composite on demand.
#[must_use]
pub fn parse_composite_id_list(s: &str) -> Option<Vec<&str>> {
    if !s.contains(',') {
        return None;
    }
    let parts: Vec<&str> = s
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    if parts.len() < 2 { None } else { Some(parts) }
}

#[async_trait::async_trait]
pub trait TileSource: Send + Sync + 'static {
    fn id(&self) -> &str;

    fn source_kind(&self) -> SourceKind {
        SourceKind::Other
    }

    /// Stable fingerprint of this source's underlying content.
    ///
    /// Cold-cache keys embed this as `cv={hex}` so that replacing a
    /// source's content (e.g. swapping a COG file) renders all prior
    /// cached tiles unreachable.
    ///
    /// The default hashes only `self.id()`, which is **safe but useless**
    /// for invalidation. Source implementations whose content can change
    /// without the id changing MUST override this with a digest of the
    /// content identity (ETag, file mtime+size, table OID + migration id).
    fn content_version(&self) -> crate::cache::ContentVersion {
        crate::cache::ContentVersion::from_input(self.id().as_bytes())
    }

    fn tilejson(&self) -> &tilejson::TileJSON;
    fn tile_info(&self) -> TileInfo;
    fn clone_source(&self) -> Box<dyn TileSource>;

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

    /// Read an ordered prefix of decoded tiles without the manager's online cache.
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

    fn supports_raster_coverage(&self) -> bool {
        false
    }

    async fn raster_coverage(
        &self,
        _query: Option<&raster_coverage::CoverageQuery>,
    ) -> MaptileResult<raster_coverage::CoverageResponse> {
        Err(crate::error::MaptileError::Config(
            "source does not support native coverage export".into(),
        ))
    }

    fn supports_raster_point(&self) -> bool {
        false
    }

    /// Query a raw raster pixel value at a coordinate. Vector and non-raster
    /// sources keep the default unsupported implementation.
    async fn query_raster_point(
        &self,
        _query: &RasterPointQuery,
    ) -> MaptileResult<RasterPointResponse> {
        Err(crate::error::MaptileError::Config(format!(
            "source '{}' does not support raster point queries",
            self.id()
        )))
    }

    /// Validator for a fetched or cached tile. Archives can use their content identity.
    fn tile_etag(&self, response: &TileResponse) -> String {
        compute_default_etag(&response.data)
    }

    /// Default implementation: call `get_tile` then compute SHA-1 of the bytes
    /// as the ETag. Source implementations that have a cheaper authoritative
    /// ETag source (postgres LSN, pmtiles blob hash, ...) should override.
    async fn get_tile_with_etag(
        &self,
        xyz: TileCoord,
        query: Option<&TileQuery<'_>>,
        if_none_match: Option<&str>,
    ) -> MaptileResult<TileEtagResponse> {
        let resp = self.get_tile(xyz, query).await?;
        let etag = self.tile_etag(&resp);
        if Some(etag.as_str()) == if_none_match {
            return Ok(TileEtagResponse::NotModified);
        }
        Ok(TileEtagResponse::Fresh { resp, etag })
    }

    /// Whether the source consumes `TileQuery.url_query`.
    fn supports_url_query(&self) -> bool {
        false
    }

    /// Whether the source consumes `TileQuery.filter` (CQL2).
    fn supports_filter(&self) -> bool {
        false
    }

    /// Persisted mosaic pixel-selection fallback. Non-mosaic sources return
    /// `None`, which also marks request-level mosaic selection unsupported.
    fn mosaic_pixel_selection(&self) -> Option<MosaicPixelSelection> {
        None
    }

    /// Zoom-level bounds for tile caching. Returned by every source
    /// to tell [`crate::sources::manager::SourceManager`] which zoom
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

/// Pixel-composition algorithms accepted by mosaic tile requests.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MosaicPixelSelection {
    First,
    Highest,
    Lowest,
    Mean,
    Median,
}

impl MosaicPixelSelection {
    #[must_use]
    pub(crate) const fn cache_tag(self) -> u8 {
        match self {
            Self::First => 1,
            Self::Highest => 2,
            Self::Lowest => 3,
            Self::Mean => 4,
            Self::Median => 5,
        }
    }
}

/// Resolve the request override against a mosaic source's persisted fallback.
#[must_use]
pub fn effective_pixel_selection(
    query: Option<&TileQuery<'_>>,
    fallback: MosaicPixelSelection,
) -> MosaicPixelSelection {
    query.and_then(|q| q.pixel_selection).unwrap_or(fallback)
}

#[derive(Debug, Clone, PartialEq)]
pub struct RasterPointQuery {
    pub x: f64,
    pub y: f64,
    pub coord_crs: String,
    /// 1-based band indices. Empty means all bands.
    pub bidx: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RasterPointResponse {
    pub x: f64,
    pub y: f64,
    pub coord_crs: String,
    pub bidx: Vec<u32>,
    pub values: Vec<f64>,
    pub valid: bool,
    pub outside: bool,
}

#[derive(Debug, Default)]
pub struct TileQuery<'a> {
    pub url_query: Option<&'a UrlQuery>,
    pub filter: Option<&'a Cql2Expr>,
    /// Spatial bbox passed via URL `?bbox=w,s,e,n`. Routed outside CQL2
    /// (see spec D6) and ANDed into source SQL as
    /// `geom && ST_MakeEnvelope(w,s,e,n,4326)`.
    pub bbox: Option<[f64; 4]>,
    /// Per-request raster styling. Joins the tile cache key by
    /// [`crate::raster::style::EffectiveStyle::style_hash`]; does not
    /// bypass the cache. Raster sources reject requests without this value.
    pub style: Option<&'a crate::raster::style::EffectiveStyle>,
    /// Dataset-authoritative hash for a complete typed raster request.
    pub style_hash: Option<crate::cache::StyleHash>,
    /// Per-request mosaic composition. Borrowed query state carries this
    /// `Copy` value without cloning; non-mosaic sources reject it.
    pub pixel_selection: Option<MosaicPixelSelection>,
}

impl<'a> TileQuery<'a> {
    /// True when the request carries inputs that make the response
    /// non-cacheable as a whole — currently `url_query`, `filter`, and
    /// `bbox` (these alter the data, not just the rendering). `style` is
    /// excluded: it joins the cache key instead.
    #[must_use]
    pub fn bypasses_cache(&self) -> bool {
        self.url_query.is_some() || self.filter.is_some() || self.bbox.is_some()
    }
}

#[derive(Debug, Clone)]
pub struct TileResponse {
    pub data: bytes::Bytes,
    pub info: TileInfo,
    pub last_modified: Option<std::time::SystemTime>,
}

#[derive(Debug)]
pub enum TileEtagResponse {
    Fresh { resp: TileResponse, etag: String },
    NotModified,
}

pub(crate) fn validate_tile_batch(coords: &[TileCoord]) -> MaptileResult<()> {
    if coords.len() > MAX_TILE_BATCH_COORDS {
        return Err(crate::error::MaptileError::InvalidRequest(format!(
            "tile batch exceeds {MAX_TILE_BATCH_COORDS} coordinates"
        )));
    }
    Ok(())
}

pub(crate) async fn read_tile_batch_sequential<S: TileSource + ?Sized>(
    source: &S,
    coords: &[TileCoord],
    query: Option<&TileQuery<'_>>,
) -> MaptileResult<Vec<TileResponse>> {
    read_tile_batch_with(coords, source.tile_info(), |coord| {
        source.get_tile(coord, query)
    })
    .await
}

pub(crate) async fn read_tile_batch_with<F, Fut>(
    coords: &[TileCoord],
    info: TileInfo,
    mut fetch: F,
) -> MaptileResult<Vec<TileResponse>>
where
    F: FnMut(TileCoord) -> Fut + Send,
    Fut: std::future::Future<Output = MaptileResult<TileResponse>> + Send,
{
    validate_tile_batch(coords)?;
    let mut tiles = Vec::with_capacity(coords.len());
    let mut bytes = 0;
    for &coord in coords {
        let mut tile = match fetch(coord).await {
            Ok(tile) => tile,
            Err(crate::error::MaptileError::TileNotFound(..)) => TileResponse {
                data: bytes::Bytes::new(),
                info,
                last_modified: None,
            },
            Err(error) => return Err(error),
        };
        if !tile.data.is_empty() {
            tile.data = crate::transport::decode_async(tile.data, tile.info.encoding).await?;
        }
        tile.info.encoding = TileEncoding::Uncompressed;
        bytes += tile.data.len();
        tiles.push(tile);
        if bytes >= MAX_TILE_BATCH_BYTES {
            break;
        }
    }
    Ok(tiles)
}

fn compute_default_etag(data: &bytes::Bytes) -> String {
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let hash: u128 = if data.is_empty() {
        0
    } else {
        xxhash_rust::xxh3::xxh3_128(data)
    };
    let mut s = String::with_capacity(24);
    s.push('"');
    s.push_str(&URL_SAFE_NO_PAD.encode(hash.to_ne_bytes()));
    s.push('"');
    s
}

#[cfg(test)]
mod composite_url_tests {
    use super::{
        MosaicPixelSelection, TileQuery, effective_pixel_selection, parse_composite_id_list,
    };

    #[test]
    fn request_selection_overrides_source_fallback() {
        let query = TileQuery {
            pixel_selection: Some(MosaicPixelSelection::Median),
            ..TileQuery::default()
        };
        assert_eq!(
            effective_pixel_selection(Some(&query), MosaicPixelSelection::First),
            MosaicPixelSelection::Median
        );
        assert_eq!(
            effective_pixel_selection(None, MosaicPixelSelection::First),
            MosaicPixelSelection::First
        );
    }

    #[test]
    fn single_id_returns_none() {
        assert_eq!(parse_composite_id_list("foo"), None);
    }

    #[test]
    fn comma_separated_returns_components() {
        assert_eq!(
            parse_composite_id_list("foo,bar,baz"),
            Some(vec!["foo", "bar", "baz"])
        );
    }

    #[test]
    fn whitespace_around_components_is_stripped() {
        assert_eq!(
            parse_composite_id_list("foo , bar , baz "),
            Some(vec!["foo", "bar", "baz"])
        );
    }

    #[test]
    fn trailing_comma_with_one_id_returns_none() {
        // Only one non-empty component → caller treats as plain id.
        assert_eq!(parse_composite_id_list("foo,"), None);
        assert_eq!(parse_composite_id_list(",foo"), None);
        assert_eq!(parse_composite_id_list(",,"), None);
    }

    #[test]
    fn two_ids() {
        assert_eq!(parse_composite_id_list("a,b"), Some(vec!["a", "b"]));
    }
}

#[cfg(test)]
mod etag_tests {
    use super::compute_default_etag;
    use bytes::Bytes;

    #[test]
    fn etag_format_is_base64_url_safe_with_quotes() {
        let etag = compute_default_etag(&Bytes::from_static(b"hello"));
        assert_eq!(etag.len(), 24, "expected 22 base64 chars + 2 quotes");
        assert!(etag.starts_with('"') && etag.ends_with('"'));
        let body = &etag[1..etag.len() - 1];
        assert!(
            body.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "body must be base64 URL-safe (no = padding): {body}"
        );
    }

    #[test]
    fn etag_empty_input_is_zero_hash_base64() {
        // xxh3_128(empty) is implementation-defined but we hash 0u128 instead.
        // Result: base64(URL_SAFE_NO_PAD) of [0; 16] = "AAAAAAAAAAAAAAAAAAAAAA".
        let etag = compute_default_etag(&Bytes::new());
        assert_eq!(etag, "\"AAAAAAAAAAAAAAAAAAAAAA\"");
    }

    #[test]
    fn etag_changes_with_data() {
        let a = compute_default_etag(&Bytes::from_static(b"alpha"));
        let b = compute_default_etag(&Bytes::from_static(b"beta"));
        assert_ne!(a, b);
    }

    #[test]
    fn etag_stable_for_same_data() {
        let a = compute_default_etag(&Bytes::from_static(b"stable"));
        let b = compute_default_etag(&Bytes::from_static(b"stable"));
        assert_eq!(a, b);
    }
}
