//! PMTiles tile sources: local file, HTTP remote, and (optional) cloud object storage.

#[cfg(feature = "cloud")]
pub mod cloud;
pub mod http;
pub mod local;

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
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

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
pub(super) async fn resolve_tile_size<B, C>(
    reader: std::sync::Arc<pmtiles::AsyncPmTilesReader<B, C>>,
    tj: &mut tilejson::TileJSON,
    info: crate::sources::TileInfo,
) -> crate::error::MaptileResult<()>
where
    B: pmtiles::AsyncBackend + Send + Sync + 'static,
    C: pmtiles::DirectoryCache + Send + Sync + 'static,
{
    use crate::error::MaptileError;
    use crate::sources::archive_inspection::{declared_tile_size, set_raster_tile_size};
    use futures_util::StreamExt;
    if matches!(info.format, TileFormat::Mvt | TileFormat::Mlt) {
        return Ok(());
    }
    if let Some(size) = declared_tile_size(tj)? {
        tj.other.insert("tile_size".into(), size.into());
        return Ok(());
    }
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut entries = reader.clone().entries().take(1024);
        while let Some(entry) = entries.next().await {
            let entry = entry.map_err(MaptileError::backend)?;
            let Some(id) = entry.iter_coords().next() else {
                continue;
            };
            let Some(bytes) = reader.get_tile(id).await.map_err(MaptileError::backend)? else {
                continue;
            };
            if !bytes.is_empty() {
                return set_raster_tile_size(tj, &bytes, info);
            }
        }
        Err(MaptileError::InvalidMetadata(
            "archive has no non-empty tile within directory inspection limit".into(),
        ))
    })
    .await
    .map_err(MaptileError::backend)?
}

/// A byte-bounded cache for parsed directories; Moka merges concurrent fills.
fn directory_cache() -> pmtiles::MokaCache {
    pmtiles::MokaCache {
        cache: moka::future::Cache::builder()
            .max_capacity(16 * 1024 * 1024)
            .weigher(|_: &usize, directory: &pmtiles::Directory| {
                directory
                    .get_approx_byte_size()
                    .saturating_add(std::mem::size_of::<pmtiles::Directory>())
                    .saturating_add(128)
                    .try_into()
                    .unwrap_or(u32::MAX)
            })
            .build(),
    }
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
fn archive_content_version(
    path: &str,
    validator: Option<&str>,
    length: Option<u64>,
) -> ContentVersion {
    static GENERATION: AtomicU64 = AtomicU64::new(0);
    let mut hash = blake3::Hasher::new();
    hash.update(b"pmtiles-content-v1");
    hash.update(&(path.len() as u64).to_le_bytes());
    hash.update(path.as_bytes());
    if let (Some(validator), Some(length)) = (validator.filter(|v| !v.is_empty()), length) {
        hash.update(b"metadata");
        hash.update(&(validator.len() as u64).to_le_bytes());
        hash.update(validator.as_bytes());
        hash.update(&length.to_le_bytes());
    } else {
        hash.update(b"generation");
        hash.update(&std::process::id().to_le_bytes());
        hash.update(
            &SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
                .to_le_bytes(),
        );
        hash.update(&GENERATION.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    }
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

async fn read_tilejson<B, C>(
    reader: std::sync::Arc<pmtiles::AsyncPmTilesReader<B, C>>,
    description: Option<&str>,
    native_info: TileInfo,
) -> MaptileResult<tilejson::TileJSON>
where
    B: pmtiles::AsyncBackend + Send + Sync + 'static,
    C: pmtiles::DirectoryCache + Send + Sync + 'static,
{
    let mut tilejson = reader.parse_tilejson(vec![]).await.map_err(|error| {
        MaptileError::InvalidMetadata(format!("PMTiles TileJSON is invalid: {error}"))
    })?;
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
        let first = archive_content_version("archive.pmtiles", Some("etag:first"), Some(100));
        assert_eq!(
            first,
            archive_content_version("archive.pmtiles", Some("etag:first"), Some(100))
        );
        assert_ne!(
            first,
            archive_content_version("archive.pmtiles", Some("etag:second"), Some(100))
        );
        assert_ne!(
            first,
            archive_content_version("archive.pmtiles", Some("etag:first"), Some(101))
        );
        assert_ne!(
            archive_content_version("archive.pmtiles", None, Some(100)),
            archive_content_version("archive.pmtiles", None, Some(100))
        );
    }

    #[tokio::test]
    async fn directory_fills_are_coalesced_and_count_empty_metadata() {
        use pmtiles::DirectoryCache;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let cache = directory_cache();
        let opens = AtomicUsize::new(0);
        let fetch = || async {
            opens.fetch_add(1, Ordering::Relaxed);
            tokio::task::yield_now().await;
            Ok(pmtiles::Directory::default())
        };
        let id = pmtiles::TileId::new(0).unwrap();
        let (a, b) = tokio::join!(
            cache.get_dir_entry_or_insert(0, id, fetch()),
            cache.get_dir_entry_or_insert(0, id, fetch())
        );
        assert!(a.unwrap().is_none());
        assert!(b.unwrap().is_none());
        assert_eq!(opens.load(Ordering::Relaxed), 1);
        cache.cache.run_pending_tasks().await;
        assert!(cache.cache.weighted_size() > 0);
    }
}
