//! Request options, coordinate validation and bounded sequential tile batches.
use super::{TileCoord, TileEncoding, TileInfo, TileResponse, TileSource};
use crate::core::tile_math::MAX_ZOOM;
use crate::error::{MaptileError, MaptileResult};
use crate::filter::{Cql2Expr, UrlQuery};

pub const MAX_TILE_BATCH_COORDS: usize = 128;
pub const MAX_TILE_BATCH_BYTES: usize = 8 * 1024 * 1024;

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
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "first" => Self::First,
            "highest" => Self::Highest,
            "lowest" => Self::Lowest,
            "mean" => Self::Mean,
            "median" => Self::Median,
            _ => return None,
        })
    }

    #[cfg(feature = "agtiff")]
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::First => "first",
            Self::Highest => "highest",
            Self::Lowest => "lowest",
            Self::Mean => "mean",
            Self::Median => "median",
        }
    }

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

/// Resolve the query override, the Raster Style reducer, then the source fallback.
#[must_use]
pub fn effective_pixel_selection(
    query: Option<&TileQuery<'_>>,
    fallback: MosaicPixelSelection,
) -> MosaicPixelSelection {
    query
        .and_then(|q| {
            q.pixel_selection.or_else(|| {
                let name = q
                    .style?
                    .raster_style()
                    .as_value()
                    .pointer("/raster:mosaic/pixel_selection")?
                    .as_str()?;
                MosaicPixelSelection::from_name(name)
            })
        })
        .unwrap_or(fallback)
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
    /// Per-request raster styling. Participates in the cache identity.
    /// Dynamic raster renderers require this value.
    pub style: Option<&'a crate::raster::style::EffectiveStyle>,
    /// Dataset-authoritative hash for a complete typed raster request.
    /// Only sources that support per-request raster styling accept it.
    pub style_hash: Option<crate::cache::StyleHash>,
    /// Per-request mosaic composition. Borrowed query state carries this
    /// `Copy` value without cloning; the manager rejects it for non-mosaic sources
    /// in both ordinary and ETag reads. Direct source callers must check capabilities.
    pub pixel_selection: Option<MosaicPixelSelection>,
}

impl<'a> TileQuery<'a> {
    pub(crate) fn has_raster_style(&self) -> bool {
        self.style.is_some() || self.style_hash.is_some()
    }

    /// True when the request carries inputs that make the response
    /// non-cacheable as a whole — currently `url_query`, `filter`, and
    /// `bbox` (these alter the data, not just the rendering). `style` is
    /// excluded: it joins the cache key instead.
    #[must_use]
    pub fn bypasses_cache(&self) -> bool {
        self.url_query.is_some() || self.filter.is_some() || self.bbox.is_some()
    }
}

/// Validate XYZ and spatial input before routing or any backend read.
pub(crate) fn validate_tile_request(
    coords: &[TileCoord],
    query: Option<&TileQuery<'_>>,
) -> Result<(), MaptileError> {
    validate_tile_batch(coords)?;
    for &TileCoord { z, x, y } in coords {
        if z > MAX_ZOOM || x >= (1u32 << z) || y >= (1u32 << z) {
            return Err(MaptileError::InvalidRequest(format!(
                "invalid tile {z}/{x}/{y}: expected z <= {MAX_ZOOM} and x,y < 2^z"
            )));
        }
    }
    if let Some(bbox) = query.and_then(|q| q.bbox)
        && !bbox.iter().all(|value| value.is_finite())
    {
        return Err(MaptileError::InvalidRequest(
            "bbox coordinates must be finite".into(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_source_request<S: TileSource + ?Sized>(
    source: &S,
    coords: &[TileCoord],
    query: Option<&TileQuery<'_>>,
) -> MaptileResult<()> {
    validate_tile_request(coords, query)?;
    if query.is_some() {
        crate::control::SourceCapabilities::from_source(source)
            .validate_query(source.id(), query)?;
    }
    Ok(())
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
    validate_source_request(source, coords, query)?;
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

#[cfg(test)]
mod tests {
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

    #[test]
    fn request_validation_preserves_finite_envelopes_and_coordinate_edges() {
        use super::{MAX_ZOOM, TileCoord, validate_tile_request};
        let last = (1 << MAX_ZOOM) - 1;
        let coords = [
            TileCoord { z: 0, x: 0, y: 0 },
            TileCoord {
                z: MAX_ZOOM,
                x: last,
                y: last,
            },
        ];
        for bbox in [
            [-180.0, -90.0, 180.0, 90.0],
            [170.0, -10.0, -170.0, 10.0],
            [0.0; 4],
            [-190.0, -91.0, 190.0, 91.0],
        ] {
            let query = TileQuery {
                bbox: Some(bbox),
                ..Default::default()
            };
            validate_tile_request(&coords, Some(&query)).unwrap();
        }
    }
}
