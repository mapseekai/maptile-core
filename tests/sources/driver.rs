use async_trait::async_trait;
use bytes::Bytes;
use maptile_core::cache::ContentVersion;
use maptile_core::control::TileDriver;
use maptile_core::error::MaptileResult;
use maptile_core::sources::{TileCoord, TileQuery, TileResponse, TileSource};
use maptile_core::sources::{TileEncoding, TileFormat, TileInfo};
use tilejson::{TileJSON, tilejson};

struct MockSource {
    id: String,
    revision: ContentVersion,
    body: Bytes,
    tj: TileJSON,
    encoding: TileEncoding,
    coordinate_payload: bool,
    missing_x: Option<u32>,
}

impl MockSource {
    fn new(id: &str, body: &[u8]) -> Self {
        Self {
            id: id.to_string(),
            revision: ContentVersion::new_generation(),
            body: Bytes::copy_from_slice(body),
            tj: tilejson! { tiles: vec![] },
            encoding: TileEncoding::Uncompressed,
            coordinate_payload: false,
            missing_x: None,
        }
    }
}

#[async_trait]
impl TileSource for MockSource {
    fn id(&self) -> &str {
        &self.id
    }
    fn tilejson(&self) -> &TileJSON {
        &self.tj
    }
    fn tile_info(&self) -> TileInfo {
        TileInfo {
            format: TileFormat::Mvt,
            encoding: self.encoding,
        }
    }
    fn content_version(&self) -> ContentVersion {
        self.revision
    }
    async fn get_tile(
        &self,
        xyz: TileCoord,
        _query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<TileResponse> {
        if self.missing_x == Some(xyz.x) {
            return Err(maptile_core::error::MaptileError::TileNotFound(
                xyz.z, xyz.x, xyz.y,
            ));
        }
        Ok(TileResponse {
            data: if self.coordinate_payload {
                Bytes::from(format!("{}/{}/{}", xyz.z, xyz.x, xyz.y))
            } else {
                self.body.clone()
            },
            info: self.tile_info(),
            last_modified: None,
        })
    }
}

#[tokio::test]
async fn batch_default_preserves_coordinate_order_and_decodes_payloads() {
    let mut source = MockSource::new("batch", b"");
    source.coordinate_payload = true;
    let coords = [
        TileCoord { z: 2, x: 3, y: 1 },
        TileCoord { z: 2, x: 0, y: 2 },
    ];
    let tiles = source.read_tile_batch(&coords, None).await.unwrap();
    assert_eq!(tiles[0].data.as_ref(), b"2/3/1");
    assert_eq!(tiles[1].data.as_ref(), b"2/0/2");
    let gzip = maptile_core::transport::encode_as(b"hello", TileEncoding::Gzip).unwrap();
    let mut source = MockSource::new("gzip", &gzip);
    source.encoding = TileEncoding::Gzip;
    let tiles = source.read_tile_batch(&coords, None).await.unwrap();
    assert_eq!(tiles[0].data.as_ref(), b"hello");
    assert_eq!(tiles[0].info.encoding, TileEncoding::Uncompressed);
}

#[tokio::test]
async fn batch_default_keeps_missing_compressed_tiles_empty_and_in_position() {
    let gzip = maptile_core::transport::encode_as(b"present", TileEncoding::Gzip).unwrap();
    let mut source = MockSource::new("sparse-gzip", &gzip);
    source.encoding = TileEncoding::Gzip;
    source.missing_x = Some(1);
    let coords = [
        TileCoord { z: 2, x: 0, y: 0 },
        TileCoord { z: 2, x: 1, y: 0 },
        TileCoord { z: 2, x: 2, y: 0 },
    ];
    let tiles = source.read_tile_batch(&coords, None).await.unwrap();
    assert_eq!(tiles.len(), 3);
    assert_eq!(tiles[0].data.as_ref(), b"present");
    assert!(tiles[1].data.is_empty());
    assert_eq!(tiles[2].data.as_ref(), b"present");
    assert!(
        tiles
            .iter()
            .all(|tile| tile.info.encoding == TileEncoding::Uncompressed)
    );
}

#[tokio::test]
async fn batch_default_returns_a_bounded_prefix_and_can_resume() {
    let source = MockSource::new("large", &vec![7; 5 * 1024 * 1024]);
    let coords = [TileCoord { z: 0, x: 0, y: 0 }; 3];
    let first = source.read_tile_batch(&coords, None).await.unwrap();
    assert_eq!(first.len(), 2);
    let second = source
        .read_tile_batch(&coords[first.len()..], None)
        .await
        .unwrap();
    assert_eq!(second.len(), 1);
    assert!(source.read_tile_batch(&[], None).await.unwrap().is_empty());
    assert!(
        source
            .read_tile_batch(&vec![coords[0]; 129], None)
            .await
            .is_err()
    );
}

#[test]
fn capability_defaults_are_false() {
    let src = MockSource::new("m", b"x");
    assert!(!src.supports_url_query());
    assert!(!src.supports_filter());
    assert!(!src.supports_raster_coverage());
    assert!(!src.supports_raster_point());
}

#[tokio::test]
async fn unsupported_raster_capabilities_are_invalid_requests() {
    use maptile_core::error::MaptileError;
    use maptile_core::sources::RasterPointQuery;

    let source = MockSource::new("vector", b"tile");
    assert!(matches!(
        source.raster_coverage(None).await,
        Err(MaptileError::InvalidRequest(_))
    ));
    assert!(matches!(
        source
            .query_raster_point(&RasterPointQuery {
                x: 0.0,
                y: 0.0,
                coord_crs: "EPSG:4326".into(),
                bidx: vec![],
            })
            .await,
        Err(MaptileError::InvalidRequest(_))
    ));
}

#[test]
fn descriptor_uses_the_revision_provided_by_the_open_source() {
    let mut source = MockSource::new("same-id", b"old");
    let reopened = MockSource::new("same-id", b"new");
    let revision = source.content_version();
    assert_eq!(source.descriptor().revision, revision);
    assert_ne!(reopened.descriptor().revision, revision);

    source.revision = ContentVersion::from_input(source.id.as_bytes());
    assert_eq!(source.descriptor().revision, source.revision);
}

#[test]
fn content_version_uses_input_bytes() {
    use maptile_core::cache::ContentVersion;
    // Sanity for the (path, mtime, size) override pattern used by mbtiles:
    // the helper must fingerprint each byte difference distinctly.
    let a = ContentVersion::from_input(
        b"file:///a.mbtiles\0\x05\x00\x00\x00\x00\x00\x00\x00\x64\x00\x00\x00\x00\x00\x00\x00",
    );
    let b = ContentVersion::from_input(
        b"file:///a.mbtiles\0\x06\x00\x00\x00\x00\x00\x00\x00\x64\x00\x00\x00\x00\x00\x00\x00",
    );
    let c = ContentVersion::from_input(
        b"file:///b.mbtiles\0\x05\x00\x00\x00\x00\x00\x00\x00\x64\x00\x00\x00\x00\x00\x00\x00",
    );
    assert_ne!(a, b, "same path different mtime should differ");
    assert_ne!(a, c, "different path same mtime should differ");
}
