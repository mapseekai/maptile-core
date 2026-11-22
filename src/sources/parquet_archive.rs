//! Local Arrow/Parquet archives with a bounded index of pre-encoded tiles.

use super::{
    SourceKind, TileCoord, TileEncoding, TileFormat, TileInfo, TileQuery, TileResponse, TileSource,
};
use crate::io::file_stamp::FileStamp;
use crate::{
    cache::{CacheZoomRange, ContentVersion},
    error::{MaptileError, MaptileResult},
};
use bytes::Bytes;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Debug, Clone)]
pub struct ParquetArchiveSourceConfig {
    pub id: String,
    pub path: PathBuf,
    pub format: TileFormat,
    pub encoding: TileEncoding,
    pub minzoom: u8,
    pub maxzoom: u8,
    pub bounds: Option<[f64; 4]>,
    pub name: Option<String>,
    pub attribution: Option<String>,
    pub description: Option<String>,
    pub columns: ParquetArchiveColumns,
    pub memory_limit_mb: usize,
    pub max_tile_bytes: usize,
    pub cache_zoom: Option<CacheZoomRange>,
}

impl Default for ParquetArchiveSourceConfig {
    fn default() -> Self {
        Self {
            id: "parquet-archive".into(),
            path: PathBuf::new(),
            format: TileFormat::Mvt,
            encoding: TileEncoding::Uncompressed,
            minzoom: 0,
            maxzoom: 22,
            bounds: None,
            name: None,
            attribution: None,
            description: None,
            columns: ParquetArchiveColumns::default(),
            memory_limit_mb: 256,
            max_tile_bytes: 8 * 1024 * 1024,
            cache_zoom: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ParquetArchiveColumns {
    pub z: String,
    pub x: String,
    pub y: String,
    pub tile: String,
}

impl Default for ParquetArchiveColumns {
    fn default() -> Self {
        Self {
            z: "z".into(),
            x: "x".into(),
            y: "y".into(),
            tile: "tile".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ParquetArchiveSource {
    id: String,
    info: TileInfo,
    tj: tilejson::TileJSON,
    tiles: Arc<HashMap<(u8, u32, u32), Bytes>>,
    revision: ContentVersion,
    cache_zoom: CacheZoomRange,
    closed: Arc<AtomicBool>,
}

impl ParquetArchiveSource {
    /// Open on a blocking thread. Once loaded, the index owns the selected bytes.
    pub fn open(cfg: ParquetArchiveSourceConfig) -> MaptileResult<Self> {
        if !cfg.path.is_file() {
            return Err(MaptileError::Config(
                "archive path must be a regular file".into(),
            ));
        }
        if cfg.minzoom > cfg.maxzoom
            || cfg.maxzoom > 30
            || !(32..=65_536).contains(&cfg.memory_limit_mb)
            || !(1..=256 * 1024 * 1024).contains(&cfg.max_tile_bytes)
            || cfg.bounds.is_some_and(|[w, s, e, n]| {
                !(-180.0..=e).contains(&w) || e > 180.0 || !(-90.0..=n).contains(&s) || n > 90.0
            })
        {
            return Err(MaptileError::Config(
                "invalid Parquet archive limits, zoom range, or bounds".into(),
            ));
        }
        let stamp = FileStamp::read(&cfg.path)?;
        let tiles = read_tile_archive(&cfg, &stamp);
        if !matches!(FileStamp::read(&cfg.path), Ok(current) if current == stamp) {
            return Err(MaptileError::VersionChanged("Parquet archive file".into()));
        }
        let tiles = tiles?;
        let revision = ContentVersion::from_input(format!("{cfg:?}:{stamp:?}").as_bytes());
        let mut tj = tilejson::tilejson! { tiles: vec![] };
        (tj.minzoom, tj.maxzoom) = (Some(cfg.minzoom), Some(cfg.maxzoom));
        tj.bounds = cfg
            .bounds
            .map(|[w, s, e, n]| tilejson::Bounds::new(w, s, e, n));
        tj.name = cfg.name;
        tj.attribution = cfg.attribution;
        tj.description = cfg.description;
        let info = TileInfo {
            format: cfg.format,
            encoding: cfg.encoding,
        };
        if !matches!(info.format, TileFormat::Mvt | TileFormat::Mlt)
            && let Some((_, bytes)) = tiles
                .iter()
                .filter(|(_, tile)| !tile.is_empty())
                .min_by_key(|(xyz, _)| *xyz)
        {
            super::archive_inspection::set_raster_tile_size(&mut tj, bytes, info)?;
        }
        Ok(Self {
            id: cfg.id,
            tj,
            tiles: Arc::new(tiles),
            revision,
            info,
            cache_zoom: cfg.cache_zoom.unwrap_or_default(),
            closed: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn tile_count(&self) -> usize {
        self.tiles.len()
    }
}

#[async_trait::async_trait]
impl TileSource for ParquetArchiveSource {
    fn id(&self) -> &str {
        &self.id
    }
    fn source_kind(&self) -> SourceKind {
        SourceKind::ParquetArchive
    }
    fn consistent_export(&self) -> bool {
        true
    }

    fn content_version(&self) -> ContentVersion {
        self.revision
    }
    fn tilejson(&self) -> &tilejson::TileJSON {
        &self.tj
    }
    fn tile_info(&self) -> TileInfo {
        self.info
    }
    fn cache_zoom(&self) -> CacheZoomRange {
        self.cache_zoom
    }
    fn clone_source(&self) -> Box<dyn TileSource> {
        Box::new(self.clone())
    }
    fn start_close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    async fn get_tile(
        &self,
        xyz: TileCoord,
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<TileResponse> {
        super::validate_source_request(self, &[xyz], query)?;
        if self.closed.load(Ordering::Acquire) {
            return Err(MaptileError::Cancelled);
        }
        if query.is_some_and(TileQuery::bypasses_cache) {
            return Err(MaptileError::Unsupported("Parquet archive filters".into()));
        }
        if self.tj.minzoom.is_some_and(|min| min > xyz.z)
            || self.tj.maxzoom.is_some_and(|max| max < xyz.z)
        {
            return Err(MaptileError::TileNotFound(xyz.z, xyz.x, xyz.y));
        }
        self.tiles
            .get(&(xyz.z, xyz.x, xyz.y))
            .map(|data| TileResponse {
                data: data.clone(),
                info: self.info,
                last_modified: None,
            })
            .ok_or(MaptileError::TileNotFound(xyz.z, xyz.x, xyz.y))
    }
}

/// Hard cap on rows scanned, including duplicates and nulls.
const PARQUET_MAX_TILES: usize = 4_000_000;

fn read_tile_archive(
    cfg: &ParquetArchiveSourceConfig,
    stamp: &FileStamp,
) -> MaptileResult<HashMap<(u8, u32, u32), Bytes>> {
    use arrow_array::cast::AsArray;
    use arrow_schema::{DataType, Schema};
    use parquet::arrow::ProjectionMask;
    use parquet::arrow::arrow_reader::{
        ArrowReaderMetadata, ArrowReaderOptions, ParquetRecordBatchReaderBuilder,
    };
    use parquet::file::reader::{ChunkReader, Length};
    use std::fs::File;

    let budget = cfg.memory_limit_mb * 1024 * 1024;
    let index_budget = budget / 2;
    let metadata_limit = (budget / 8).min(16 * 1024 * 1024);
    let file = File::open(&cfg.path)?;
    if FileStamp::from_file(&file)? != *stamp {
        return Err(MaptileError::VersionChanged("Parquet archive file".into()));
    }
    let footer_offset = file
        .len()
        .checked_sub(8)
        .ok_or_else(|| MaptileError::InvalidMetadata("truncated Parquet footer".into()))?;
    let footer = file
        .get_bytes(footer_offset, 8)
        .map_err(MaptileError::backend)?;
    let metadata_len = u32::from_le_bytes(footer[..4].try_into().unwrap()) as usize;
    if metadata_len > metadata_limit {
        return Err(MaptileError::PayloadTooLarge {
            limit: metadata_limit,
        });
    }
    let builder = ParquetRecordBatchReaderBuilder::try_new(file.try_clone()?)
        .map_err(MaptileError::backend)?;

    let schema = builder.schema().clone();
    let columns = &cfg.columns;
    let z_idx = column_index(&schema, &columns.z)?;
    let x_idx = column_index(&schema, &columns.x)?;
    let y_idx = column_index(&schema, &columns.y)?;
    let tile_idx = column_index(&schema, &columns.tile)?;
    if !matches!(
        schema.field(tile_idx).data_type(),
        DataType::Binary | DataType::LargeBinary | DataType::BinaryView
    ) {
        return Err(MaptileError::Config(format!(
            "parquet archive: column {:?} must contain binary tile bytes",
            columns.tile,
        )));
    }
    let mut selected = vec![z_idx, x_idx, y_idx, tile_idx];
    selected.sort_unstable();
    selected.dedup();
    if selected.len() != 4 {
        return Err(MaptileError::Config(
            "archive columns must be distinct".into(),
        ));
    }
    let parquet_schema = builder.parquet_schema();
    let mask = ProjectionMask::roots(parquet_schema, selected.iter().copied());
    let file_metadata = builder.metadata().clone();
    let rows = file_metadata
        .row_groups()
        .iter()
        .try_fold(0usize, |total, group| {
            let rows = usize::try_from(group.num_rows()).ok()?;
            total.checked_add(rows)
        });
    if rows.is_none_or(|rows| rows > PARQUET_MAX_TILES) {
        return Err(MaptileError::Config(format!(
            "parquet archive: archive exceeds {PARQUET_MAX_TILES} tile rows; use PMTiles/MBTiles",
        )));
    }
    for group in file_metadata.row_groups() {
        let mut bytes = 0usize;
        for (_, column) in group
            .columns()
            .iter()
            .enumerate()
            .filter(|(i, _)| mask.leaf_included(*i))
        {
            // Cargo feature unification must not enable eager decompression on this path.
            if column.compression() != parquet::basic::Compression::UNCOMPRESSED {
                return Err(MaptileError::InvalidMetadata(
                    "pre-encoded Parquet archive columns must be uncompressed".into(),
                ));
            }
            let size = column.uncompressed_size().max(column.compressed_size());
            bytes = bytes.saturating_add(usize::try_from(size).unwrap_or(usize::MAX));
            if bytes > budget / 4 {
                return Err(MaptileError::PayloadTooLarge { limit: budget / 4 });
            }
        }
    }
    // BinaryView keeps repeated dictionary entries borrowed until we check the tile budget.
    let mut fields = schema.fields().to_vec();
    fields[tile_idx] = Arc::new(
        schema
            .field(tile_idx)
            .clone()
            .with_data_type(DataType::BinaryView),
    );
    let options = ArrowReaderOptions::new().with_schema(Arc::new(Schema::new(fields)));
    let metadata =
        ArrowReaderMetadata::try_new(file_metadata, options).map_err(MaptileError::backend)?;
    let reader = ParquetRecordBatchReaderBuilder::new_with_metadata(file, metadata)
        .with_projection(mask)
        .build()
        .map_err(MaptileError::backend)?;
    let [z_idx, x_idx, y_idx, tile_idx] = [z_idx, x_idx, y_idx, tile_idx]
        .map(|idx| selected.binary_search(&idx).expect("selected column"));

    let mut out: HashMap<(u8, u32, u32), Bytes> = HashMap::new();
    let mut retained = 0usize;
    let mut seen_rows = 0usize;
    for batch in reader {
        let batch = batch.map_err(MaptileError::backend)?;
        seen_rows = seen_rows.saturating_add(batch.num_rows());
        if seen_rows > PARQUET_MAX_TILES {
            return Err(MaptileError::Config(format!(
                "parquet archive: archive exceeds {PARQUET_MAX_TILES} tile rows; \
                 use PMTiles/MBTiles for larger workloads"
            )));
        }

        let z_col = batch.column(z_idx);
        let x_col = batch.column(x_idx);
        let y_col = batch.column(y_idx);
        let tile_col = batch.column(tile_idx);

        let tile_bytes = tile_col
            .as_binary_view_opt()
            .ok_or_else(|| MaptileError::InvalidMetadata("expected a binary tile column".into()))?
            .iter();

        let z_iter = primitive_u32_iter(z_col)?;
        let x_iter = primitive_u32_iter(x_col)?;
        let y_iter = primitive_u32_iter(y_col)?;

        for (((z, x), y), tile) in z_iter.zip(x_iter).zip(y_iter).zip(tile_bytes) {
            let (Some(z), Some(x), Some(y), Some(tile)) = (z, x, y, tile) else {
                continue;
            };
            if z > 30 || x >= 1 << z || y >= 1 << z {
                continue;
            }
            if tile.len() > cfg.max_tile_bytes {
                return Err(MaptileError::PayloadTooLarge {
                    limit: cfg.max_tile_bytes,
                });
            }
            let key = (z as u8, x, y);
            // Include key, hash table capacity, and per-tile allocation overhead.
            retained -= out.get(&key).map_or(0, |old| old.len() + 128);
            retained = retained.saturating_add(tile.len() + 128);
            if retained > index_budget {
                return Err(MaptileError::PayloadTooLarge {
                    limit: index_budget,
                });
            }
            out.insert(key, Bytes::copy_from_slice(tile));
        }
    }
    Ok(out)
}

fn column_index(schema: &arrow_schema::Schema, name: &str) -> MaptileResult<usize> {
    schema
        .index_of(name)
        .map_err(|_| MaptileError::Config(format!("parquet archive: column {name:?} not found")))
}

/// Iterate any 8/16/32/64-bit integer column as `Option<u32>`. Returns
/// `None` for nulls, `None` for negative values (which makes no sense
/// for tile coordinates).
fn primitive_u32_iter(
    col: &dyn arrow_array::Array,
) -> MaptileResult<Box<dyn Iterator<Item = Option<u32>> + '_>> {
    use arrow_array::{cast::AsArray, types::*};
    use arrow_schema::DataType;
    macro_rules! integers {
        ($array_type:ty) => {{
            let values = col.as_primitive::<$array_type>();
            Box::new(values.iter().map(|v| v.and_then(|v| u32::try_from(v).ok())))
        }};
    }
    Ok(match col.data_type() {
        DataType::UInt8 => integers!(UInt8Type),
        DataType::UInt16 => integers!(UInt16Type),
        DataType::UInt32 => integers!(UInt32Type),
        DataType::UInt64 => integers!(UInt64Type),
        DataType::Int8 => integers!(Int8Type),
        DataType::Int16 => integers!(Int16Type),
        DataType::Int32 => integers!(Int32Type),
        DataType::Int64 => integers!(Int64Type),
        other => {
            return Err(MaptileError::Config(format!(
                "parquet archive: unsupported coordinate column type {other:?}"
            )));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn preencoded_archives_remain_compatible_and_share_the_index() {
        use arrow_array::{BinaryArray, RecordBatch, UInt8Array, UInt32Array};
        use arrow_schema::{DataType, Field, Schema};
        use parquet::arrow::ArrowWriter;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("archive.parquet");
        let schema = Arc::new(Schema::new(vec![
            Field::new("z", DataType::UInt8, false),
            Field::new("x", DataType::UInt32, false),
            Field::new("y", DataType::UInt32, false),
            Field::new("tile", DataType::Binary, false),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(UInt8Array::from(vec![0, 1])),
                Arc::new(UInt32Array::from(vec![0, 1])),
                Arc::new(UInt32Array::from(vec![0, 1])),
                Arc::new(BinaryArray::from_vec(vec![b"first", b"second"])),
            ],
        )
        .unwrap();
        let mut writer =
            ArrowWriter::try_new(std::fs::File::create(&path).unwrap(), schema, None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
        let cfg = ParquetArchiveSourceConfig {
            path,
            ..Default::default()
        };
        let source = ParquetArchiveSource::open(cfg.clone()).unwrap();
        let cloned = source.clone();
        assert!(Arc::ptr_eq(&source.tiles, &cloned.tiles));
        assert_eq!(source.tile_count(), 2);
        assert_eq!(
            source
                .get_tile(TileCoord { z: 1, x: 1, y: 1 }, None)
                .await
                .unwrap()
                .data
                .as_ref(),
            b"second"
        );
        assert!(matches!(
            source.get_tile(TileCoord { z: 1, x: 0, y: 0 }, None).await,
            Err(MaptileError::TileNotFound(..))
        ));
        let mut mislabeled = cfg.clone();
        mislabeled.format = TileFormat::Png;
        assert!(ParquetArchiveSource::open(mislabeled).is_err());
        let mut bounded = cfg;
        bounded.max_tile_bytes = 5;
        assert!(matches!(
            ParquetArchiveSource::open(bounded),
            Err(MaptileError::PayloadTooLarge { limit: 5 })
        ));
    }
}
