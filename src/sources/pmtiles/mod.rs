//! PMTiles tile sources: local file, HTTP remote, and (optional) cloud object storage.

#[cfg(feature = "cloud")]
pub mod cloud;
pub mod http;
pub mod local;
mod reader;

#[cfg(feature = "cloud")]
pub use cloud::{CloudPmtilesSource, is_cloud_url};
pub use http::HttpPmtilesSource;
pub use local::LocalPmtilesSource;

/// Re-export of the upstream PMTiles archive writer so that downstream
/// consumers building maptile-server admin / ingest endpoints don't need a
/// direct `pmtiles` dep.
#[cfg(feature = "cloud")]
pub use pmtiles::{AsyncPmTilesReader, ObjectStoreBackend};

pub use pmtiles::{Compression, PmTilesStreamWriter, PmTilesWriter, TileCoord, TileId, TileType};

use crate::cache::ContentVersion;
use crate::error::{MaptileError, MaptileResult};
use crate::sources::archive_inspection::{ArchiveSourceType, inspect_archive};
use crate::sources::{TileEncoding, TileFormat, TileInfo, TileResponse};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct PmtilesConfig {
    pub id: String,
    /// Filesystem path (local) or URL (`http://` / `https://` / `s3://` / ...).
    pub path: String,
    pub serve_as: Option<TileFormat>,
    pub description: Option<String>,
}

// Follow archive directory entries rather than guessing occupied coordinates.
// Bound malformed/empty-entry traversal and remote I/O during source opening.
async fn resolve_tile_size<B>(
    reader: &reader::Reader<B>,
    tj: &mut tilejson::TileJSON,
    info: crate::sources::TileInfo,
) -> crate::error::MaptileResult<()>
where
    B: pmtiles::AsyncBackend + Send + Sync,
{
    use crate::error::MaptileError;
    use crate::sources::archive_inspection::{declared_tile_size, set_raster_tile_size};
    if matches!(info.format, TileFormat::Mvt | TileFormat::Mlt) {
        return Ok(());
    }
    if let Some(size) = declared_tile_size(tj)? {
        tj.other.insert("tile_size".into(), size.into());
        return Ok(());
    }
    let bytes = tokio::time::timeout(Duration::from_secs(5), reader.sample_tile())
        .await
        .map_err(MaptileError::backend)??;
    set_raster_tile_size(tj, &bytes, info)
}

/// Convert archive tile types without depending on a storage backend.
fn pmt_tile_type_to_format(t: pmtiles::TileType) -> MaptileResult<TileFormat> {
    use pmtiles::TileType::*;
    match t {
        Mvt => Ok(TileFormat::Mvt),
        Mlt => Ok(TileFormat::Mlt),
        Png => Ok(TileFormat::Png),
        Jpeg => Ok(TileFormat::Jpeg),
        Webp => Ok(TileFormat::Webp),
        unsupported => Err(MaptileError::InvalidMetadata(format!(
            "unsupported PMTiles tile type: {unsupported:?}"
        ))),
    }
}

/// Convert a pmtiles `Compression` to our `TileEncoding`.
fn pmt_compression_to_encoding(c: pmtiles::Compression) -> MaptileResult<TileEncoding> {
    use pmtiles::Compression::*;
    match c {
        None => Ok(TileEncoding::Uncompressed),
        Gzip => Ok(TileEncoding::Gzip),
        Brotli => Ok(TileEncoding::Brotli),
        Zstd => Ok(TileEncoding::Zstd),
        Unknown => Err(MaptileError::InvalidMetadata(
            "unsupported PMTiles tile compression: Unknown".into(),
        )),
    }
}

/// Header bounds and zooms do not identify tile content. Prefer the archive's
/// storage validator and size; otherwise isolate this open generation so a
/// reconstructed source never reuses unverifiable cold-cache entries.
fn archive_content_version(path: &str, validator: Option<&str>) -> ContentVersion {
    let Some(validator) = validator.filter(|v| !v.is_empty()) else {
        return ContentVersion::new_generation();
    };
    let mut hash = blake3::Hasher::new();
    hash.update(b"pmtiles-content-v1");
    hash.update(&(path.len() as u64).to_le_bytes());
    hash.update(path.as_bytes());
    hash.update(b"metadata");
    hash.update(&(validator.len() as u64).to_le_bytes());
    hash.update(validator.as_bytes());
    ContentVersion::from_bytes(*hash.finalize().as_bytes())
}

/// Include both the archive identity and fetched bytes: different tiles from
/// one archive must not share a strong HTTP validator.
fn archive_tile_etag(version: ContentVersion, response: &TileResponse) -> String {
    let mut hash = blake3::Hasher::new();
    hash.update(version.to_hex().as_bytes());
    hash.update(&response.data);
    format!("\"{}\"", hash.finalize().to_hex())
}

async fn read_tilejson<B>(
    reader: &reader::Reader<B>,
    description: Option<&str>,
    native_info: TileInfo,
) -> MaptileResult<tilejson::TileJSON>
where
    B: pmtiles::AsyncBackend + Send + Sync,
{
    use serde_json::Value;
    let mut tilejson = reader.header.get_tilejson(Vec::new());
    for (key, value) in reader.metadata().await? {
        match (key.as_str(), value) {
            ("description", Value::String(text)) => tilejson.description = Some(text),
            ("attribution", Value::String(text)) => tilejson.attribution = Some(text),
            ("legend", Value::String(text)) => tilejson.legend = Some(text),
            ("name", Value::String(text)) => tilejson.name = Some(text),
            ("version", Value::String(text)) => tilejson.version = Some(text),
            ("minzoom" | "maxzoom", _) => {}
            ("vector_layers", value) => {
                tilejson.vector_layers = Some(serde_json::from_value(value).map_err(|error| {
                    MaptileError::InvalidMetadata(format!("invalid PMTiles vector_layers: {error}"))
                })?);
            }
            (_, value) => {
                tilejson.other.insert(key, value);
            }
        }
    }
    if let Some(description) = description {
        tilejson.description = Some(description.to_owned());
    }
    resolve_tile_size(reader, &mut tilejson, native_info).await?;
    Ok(inspect_archive(ArchiveSourceType::Pmtiles, &tilejson, native_info)?.tilejson)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::prelude::{PmTilesStreamWriter, PmTilesWriter};
    use crate::sources::pmtiles::{
        PmTilesStreamWriter as ModStreamWriter, PmTilesWriter as ModBuilder,
    };

    #[test]
    fn pmtiles_writer_types_resolve() {
        fn _accepts<T>() {}
        _accepts::<PmTilesWriter>();
        _accepts::<PmTilesStreamWriter<std::io::Cursor<Vec<u8>>>>();
        _accepts::<ModBuilder>();
        _accepts::<ModStreamWriter<std::io::Cursor<Vec<u8>>>>();
    }

    #[test]
    fn storage_validators_change_content_identity_and_unknown_versions_are_isolated() {
        let first = archive_content_version("archive.pmtiles", Some("100:etag:first"));
        assert_eq!(
            first,
            archive_content_version("archive.pmtiles", Some("100:etag:first"))
        );
        assert_ne!(
            first,
            archive_content_version("archive.pmtiles", Some("100:etag:second"))
        );
        assert_ne!(
            first,
            archive_content_version("archive.pmtiles", Some("101:etag:first"))
        );
        assert_ne!(
            archive_content_version("archive.pmtiles", None),
            archive_content_version("archive.pmtiles", None)
        );
    }
}
