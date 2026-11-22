//! Tile source contract and default per-source operations.
use super::{
    MosaicPixelSelection, RasterPointQuery, RasterPointResponse, TileCoord, TileEtagResponse,
    TileInfo, TileQuery, TileResponse, archive_inspection, raster_coverage,
    request::read_tile_batch_sequential, validate_source_request,
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
pub trait TileSource: std::any::Any + Send + Sync {
    fn id(&self) -> &str;

    fn source_kind(&self) -> SourceKind {
        SourceKind::Other
    }

    /// Fingerprint used to partition cached tiles by source content.
    ///
    /// Cache keys include this version so a new content identity selects a new
    /// set of entries. The default derives the fingerprint from [`Self::id`].
    /// Sources that reuse an ID across content changes must override this method
    /// with a content fingerprint, file generation, or durable edit revision.
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

    /// Validator for a fetched or cached tile. Archives can use their content identity.
    fn tile_etag(&self, response: &TileResponse) -> String {
        compute_default_etag(&response.data)
    }

    /// Fetch a tile and compare its [`Self::tile_etag`] with `if_none_match`.
    /// A match returns [`TileEtagResponse::NotModified`]; otherwise the response
    /// includes the tile and its ETag. The default validator derives from the
    /// payload, and a source can override it with an authoritative content ID.
    async fn get_tile_with_etag(
        &self,
        xyz: TileCoord,
        query: Option<&TileQuery<'_>>,
        if_none_match: Option<&str>,
    ) -> MaptileResult<TileEtagResponse> {
        validate_source_request(self, &[xyz], query)?;
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

    /// Whether the source applies a per-request [`TileQuery::style`].
    fn supports_raster_style(&self) -> bool {
        false
    }

    /// Whether the opened capability remains valid for every catalog read.
    #[doc(hidden)]
    fn supports_catalog_raster_style(&self) -> bool {
        self.supports_raster_style()
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
