//! Local FlatGeobuf tiles and one-shot GeoJSON / ZIP Shapefile imports.
//!
//! Requires `local-vector` and the same signed DuckDB Spatial extension as
//! GeoParquet. No system GDAL, remote URLs, user SQL, or driver options are used.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use duckdb::{Connection, InterruptHandle};
use tempfile::TempDir;
use tokio::sync::Notify;

use super::geoparquet::{GeoParquetQueryOptions, GeoParquetSource, GeoParquetSourceConfig};
use super::geoparquet_geometry::{
    FileStamp, GeometrySource, identifier, literal, single_file_path, spatial_connection,
    unused_name, validate_single_file_path,
};
use super::{TileCoord, TileInfo, TileQuery, TileResponse, TileSource};
use crate::error::{MaptileError, MaptileResult};

/// Local FlatGeobuf geometry source settings. Geometry and CRS are discovered
/// from the file; `srid` explicitly overrides missing or incorrect CRS metadata.
#[derive(Debug, Clone)]
pub struct FlatGeobufSourceConfig {
    pub id: String,
    pub path: PathBuf,
    pub minzoom: u8,
    pub maxzoom: u8,
    pub bounds: Option<[f64; 4]>,
    pub name: Option<String>,
    pub attribution: Option<String>,
    pub description: Option<String>,
    pub layer_name: String,
    pub srid: Option<i32>,
    pub id_column: Option<String>,
    /// None selects MVT-compatible scalar properties; Some is an allowlist.
    pub properties: Option<Vec<String>>,
    pub extent: u32,
    pub buffer: u32,
    pub query_options: GeoParquetQueryOptions,
    pub cache_zoom: Option<crate::cache::CacheZoomRange>,
}

impl Default for FlatGeobufSourceConfig {
    fn default() -> Self {
        Self {
            id: "flatgeobuf".into(),
            path: PathBuf::new(),
            minzoom: 0,
            maxzoom: 22,
            bounds: None,
            name: None,
            attribution: None,
            description: None,
            layer_name: "data".into(),
            srid: None,
            id_column: None,
            properties: None,
            extent: 4096,
            buffer: 64,
            query_options: GeoParquetQueryOptions::default(),
            cache_zoom: None,
        }
    }
}

/// On-demand MVT from one immutable local FlatGeobuf file.
///
/// Indexed and unindexed files are scanned with exact projected intersection.
/// Spatial 1.5.6's index pushdown can lose projected lines and produce invalid
/// empty batches, so this version deliberately does not use the packed index.
/// All clones share the bounded query pool, cancellation, file version, and
/// close lifecycle. Large files may exceed the configured per-tile deadline.
#[derive(Debug, Clone)]
pub struct FlatGeobufSource(GeoParquetSource);

impl FlatGeobufSource {
    /// Open on a blocking thread. CRS must have a recognized EPSG / CRS84
    /// authority or an explicit positive `srid` override.
    ///
    /// # Errors
    /// Returns an error for nonlocal / missing files, incompatible drivers,
    /// invalid configuration, missing Spatial, or unsupported schema / CRS.
    pub fn open(cfg: FlatGeobufSourceConfig) -> MaptileResult<Self> {
        let cfg = GeoParquetSourceConfig {
            id: cfg.id,
            path: normalized_local_path(single_file_path(&cfg.path)?)?,
            minzoom: cfg.minzoom,
            maxzoom: cfg.maxzoom,
            bounds: cfg.bounds,
            name: cfg.name,
            attribution: cfg.attribution,
            description: cfg.description,
            geometry_column: Some("geom".into()),
            layer_name: cfg.layer_name,
            srid: cfg.srid,
            id_column: cfg.id_column,
            properties: cfg.properties,
            extent: cfg.extent,
            buffer: cfg.buffer,
            query_options: cfg.query_options,
            cache_zoom: cfg.cache_zoom,
            ..Default::default()
        };
        let geometry = GeometrySource::open_flatgeobuf(&cfg)?;
        let meta = std::fs::metadata(&cfg.path).map_err(MaptileError::backend)?;
        let identity = format!(
            "flatgeobuf:{cfg:?}:{}:{:?}",
            meta.len(),
            meta.modified().map_err(MaptileError::backend)?
        );
        let version = crate::cache::ContentVersion::from_input(identity.as_bytes());
        GeoParquetSource::from_opened(cfg, HashMap::new(), Some(Arc::new(geometry)), version)
            .map(Self)
    }

    /// Retain ownership of the source file through clones and active queries.
    pub fn with_file_lease(self, lease: impl Send + Sync + 'static) -> Self {
        Self(self.0.with_file_lease(lease))
    }

    /// Reject new queries and begin cancelling existing queries on every clone.
    pub fn start_close(&self) {
        self.0.start_close();
    }

    /// Wait until every query has released its connection and file lease.
    pub async fn close(&self) -> MaptileResult<()> {
        self.0.close().await
    }
}

#[async_trait::async_trait]
impl TileSource for FlatGeobufSource {
    fn id(&self) -> &str {
        self.0.id()
    }
    fn content_version(&self) -> crate::cache::ContentVersion {
        self.0.content_version()
    }
    fn tilejson(&self) -> &tilejson::TileJSON {
        self.0.tilejson()
    }
    fn tile_info(&self) -> TileInfo {
        self.0.tile_info()
    }
    fn cache_zoom(&self) -> crate::cache::CacheZoomRange {
        self.0.cache_zoom()
    }
    fn clone_source(&self) -> Box<dyn TileSource> {
        Box::new(self.clone())
    }
    fn start_close(&self) {
        Self::start_close(self);
    }
    async fn close(&self) -> MaptileResult<()> {
        Self::close(self).await
    }
    async fn get_tile(
        &self,
        xyz: TileCoord,
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<TileResponse> {
        self.0.get_tile(xyz, query).await
    }
}

/// Accepted one-shot import formats. ZIP must contain exactly one Shapefile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VectorImportFormat {
    GeoJson,
    ShapefileZip,
}

/// Import limits. Each import uses two DuckDB threads, 256 MB of DuckDB-managed memory,
/// and no disk spilling. Applications must bound the number of concurrent imports.
#[derive(Debug, Clone)]
pub struct VectorImportOptions {
    /// Override the input CRS with a positive EPSG code. Normal GeoJSON is WGS84;
    /// a Shapefile without a recognized .prj needs this override.
    pub srid: Option<i32>,
    /// Maximum input bytes, total advertised ZIP expansion, actual extracted
    /// bytes, and completed output bytes, each separately (default 512 MiB).
    /// Output size is checked after COPY, before publication; this is not a disk quota.
    pub max_bytes: u64,
    /// Deadline for staging and conversion, including waiting for a worker.
    pub timeout_ms: u64,
}

impl Default for VectorImportOptions {
    fn default() -> Self {
        Self {
            srid: None,
            max_bytes: 512 * 1024 * 1024,
            timeout_ms: 30_000,
        }
    }
}

/// Published immutable GeoParquet file. Use the returned geometry column when
/// constructing a [`GeoParquetSourceConfig`]; original attribute names survive.
#[derive(Debug, Clone)]
pub struct ImportedGeoParquet {
    pub path: PathBuf,
    pub geometry_column: String,
}

/// Import a local GeoJSON file or ZIP Shapefile once, then serve its GeoParquet.
///
/// The output contains WGS84 WKB (M removed, Z retained), original attributes, and a GeoParquet 1.1 bbox
/// covering. Geometry / bbox names are chosen to avoid attribute collisions.
/// ZIP extraction uses a private directory, accepts only one .shp with matching
/// .shx / .dbf and optional .prj / .cpg, and is bounded to 128 entries. Paths,
/// duplicates, links, encrypted entries, and unsupported compression are rejected.
///
/// The destination must not exist. A complete file is atomically hard-linked
/// from a private directory in the destination's parent; no partial result is
/// published. Cancelling the future interrupts conversion and discards staging
/// when the blocking worker exits. Once this function returns, the caller owns
/// the result independently of the original inputs.
///
/// # Errors
/// Returns an error for invalid input / limits / schema / CRS, a deadline,
/// missing Spatial, an existing destination, or filesystem / conversion errors.
pub async fn import_to_geoparquet(
    input: impl AsRef<Path>,
    format: VectorImportFormat,
    output: impl AsRef<Path>,
    options: VectorImportOptions,
) -> MaptileResult<ImportedGeoParquet> {
    if !(1..=8 * 1024 * 1024 * 1024).contains(&options.max_bytes)
        || !(1..=300_000).contains(&options.timeout_ms)
        || options.srid.is_some_and(|srid| srid <= 0)
    {
        return Err(config("invalid import limits or srid"));
    }
    let input = input.as_ref().to_owned();
    let output = output.as_ref().to_owned();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(options.timeout_ms);
    let control = Arc::new(ImportControl::default());
    let _cancel = CancelImport(control.clone());
    let watchdog = control.clone();
    tokio::spawn(async move {
        tokio::select! {
            () = tokio::time::sleep_until(deadline) => {},
            () = watchdog.wake.notified() => {},
        }
        while !watchdog.done.load(Ordering::Acquire) {
            watchdog.cancelled.store(true, Ordering::Release);
            if let Some(handle) = &*watchdog
                .interrupt
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
            {
                handle.interrupt();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    let worker_control = control.clone();
    let task = tokio::task::spawn_blocking(move || {
        let _finish = FinishImport(worker_control.clone());
        import_staged(&input, format, &output, &options, &worker_control)
    });
    let staged = tokio::time::timeout_at(deadline, task)
        .await
        .map_err(|_| config("import exceeded its deadline"))?
        .map_err(MaptileError::backend)??;
    control.check()?;
    // Publication stays outside the worker: a dropped future cannot publish.
    if tokio::time::Instant::now() >= deadline {
        return Err(config("import exceeded its deadline"));
    }
    std::fs::hard_link(
        staged.dir.path().join("result.parquet"),
        &staged.result.path,
    )
    .map_err(MaptileError::backend)?;
    Ok(staged.result)
}

#[derive(Default)]
struct ImportControl {
    cancelled: AtomicBool,
    done: AtomicBool,
    interrupt: Mutex<Option<Arc<InterruptHandle>>>,
    wake: Notify,
}
impl ImportControl {
    fn check(&self) -> MaptileResult<()> {
        if self.cancelled.load(Ordering::Acquire) {
            Err(config("import was cancelled"))
        } else {
            Ok(())
        }
    }
}
struct CancelImport(Arc<ImportControl>);
impl Drop for CancelImport {
    fn drop(&mut self) {
        self.0.cancelled.store(true, Ordering::Release);
        self.0.wake.notify_one();
    }
}
struct FinishImport(Arc<ImportControl>);
impl Drop for FinishImport {
    fn drop(&mut self) {
        self.0.done.store(true, Ordering::Release);
        self.0.wake.notify_one();
    }
}
struct ClearImportInterrupt<'a>(&'a ImportControl);
impl Drop for ClearImportInterrupt<'_> {
    fn drop(&mut self) {
        // Retire the handle under the watchdog's lock before Connection drops.
        self.0
            .interrupt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }
}
struct StagedImport {
    dir: TempDir,
    result: ImportedGeoParquet,
}

fn import_staged(
    input: &Path,
    format: VectorImportFormat,
    output: &Path,
    options: &VectorImportOptions,
    control: &ImportControl,
) -> MaptileResult<StagedImport> {
    control.check()?;
    let input = normalized_local_path(single_file_path(input)?)?;
    let stamp = FileStamp::read(&input)?;
    if std::fs::metadata(&input)
        .map_err(MaptileError::backend)?
        .len()
        > options.max_bytes
    {
        return Err(config("input exceeds max_bytes"));
    }
    validate_single_file_path(output)?;
    let filename = output
        .file_name()
        .ok_or_else(|| config("destination must name a file"))?;
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .canonicalize()
        .map_err(MaptileError::backend)?;
    let parent = normalized_local_path(parent)?;
    let output = parent.join(filename);
    if std::fs::symlink_metadata(&output).is_ok() {
        return Err(config("destination already exists"));
    }
    let mut builder = tempfile::Builder::new();
    builder.prefix(".maptile-import-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    let dir = builder.tempdir_in(parent).map_err(MaptileError::backend)?;
    let (source, driver) = match format {
        VectorImportFormat::GeoJson => {
            let source = dir.path().join("input.geojson");
            copy_bounded(
                &mut File::open(&input).map_err(MaptileError::backend)?,
                &source,
                options.max_bytes,
                control,
            )?;
            (source, "GeoJSON")
        }
        VectorImportFormat::ShapefileZip => {
            let archive = dir.path().join("input.zip");
            copy_bounded(
                &mut File::open(&input).map_err(MaptileError::backend)?,
                &archive,
                options.max_bytes,
                control,
            )?;
            (
                extract_shapefile(&archive, dir.path(), options.max_bytes, control)?,
                "ESRI Shapefile",
            )
        }
    };
    if FileStamp::read(&input)? != stamp {
        return Err(config("input changed during staging"));
    }
    control.check()?;
    let conn = spatial_connection(&GeoParquetQueryOptions::default())?;
    let _clear_interrupt = ClearImportInterrupt(control);
    *control
        .interrupt
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(conn.interrupt_handle());
    control.check()?;
    let allowed = path_text(dir.path())?;
    conn.execute_batch(&format!(
        "SET allowed_directories = [{}]; SET enable_external_access = false;",
        literal(&format!("{allowed}/"))
    ))
    .map_err(MaptileError::backend)?;
    let source = path_text(&source)?;
    let metadata = vector_metadata(&conn, source, driver, options.srid)?;
    let from = format!(
        "ST_Read({}, allowed_drivers := [{}], keep_wkb := true)",
        literal(source),
        literal(driver)
    );
    let columns: BTreeMap<String, String> = conn
        .prepare(&format!("DESCRIBE SELECT * FROM {from}"))
        .and_then(|mut s| {
            s.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<_, _>>()
        })
        .map_err(MaptileError::backend)?;
    if columns.get("wkb_geometry").map(String::as_str) != Some("BLOB") {
        return Err(config("driver did not provide its WKB geometry column"));
    }
    let geometry = unused_name("geometry", &columns);
    let bbox = unused_name("__maptile_bbox", &columns);
    let geom = identifier(&geometry)?;
    let bbox_sql = identifier(&bbox)?;
    let properties = columns
        .keys()
        .filter(|name| name.as_str() != "wkb_geometry")
        .map(|name| identifier(name).map(|name| format!(", {name}")))
        .collect::<MaptileResult<String>>()?;
    let raw = "ST_GeomFromWKB(wkb_geometry)";
    let normalized = format!(
        "CASE WHEN ST_HasM({raw}) AND ST_HasZ({raw}) THEN ST_Force3DZ({raw}, 0.0) WHEN ST_HasM({raw}) THEN ST_Force2D({raw}) ELSE {raw} END"
    );
    let projected = format!(
        "ST_Transform({normalized}, 'EPSG:{}', 'EPSG:4326', always_xy := true)",
        metadata.srid
    );
    // Validate the explicit CRS even if the input is empty.
    conn.query_row(&format!("SELECT ST_AsText(ST_Transform(ST_Point(0, 0), 'EPSG:{}', 'EPSG:4326', always_xy := true))", metadata.srid), [], |r| r.get::<_, String>(0)).map_err(MaptileError::backend)?;
    let geo = serde_json::json!({
        "version": "1.1.0", "primary_column": geometry,
        "columns": { &geometry: {
            "encoding": "WKB", "geometry_types": [],
            "covering": { "bbox": {
                "xmin": [&bbox, "xmin"], "ymin": [&bbox, "ymin"],
                "xmax": [&bbox, "xmax"], "ymax": [&bbox, "ymax"]
            }}
        }}
    });
    let result_path = dir.path().join("result.parquet");
    control.check()?;
    conn.execute_batch(&format!(
        "COPY (SELECT ST_AsWKB({geom})::BLOB AS {geom}{properties}, CASE WHEN {geom} IS NOT NULL THEN struct_pack(xmin := ST_XMin({geom}), ymin := ST_YMin({geom}), xmax := ST_XMax({geom}), ymax := ST_YMax({geom})) END AS {bbox_sql} FROM (SELECT {projected} AS {geom}{properties} FROM {from})) TO {} (FORMAT PARQUET, KV_METADATA {{geo: {}}})",
        literal(path_text(&result_path)?), literal(&geo.to_string())
    )).map_err(MaptileError::backend)?;
    control.check()?;
    let result = File::options()
        .read(true)
        .write(true)
        .open(&result_path)
        .map_err(MaptileError::backend)?;
    if result.metadata().map_err(MaptileError::backend)?.len() > options.max_bytes {
        return Err(config("output exceeds max_bytes"));
    }
    result.sync_all().map_err(MaptileError::backend)?;
    Ok(StagedImport {
        dir,
        result: ImportedGeoParquet {
            path: output,
            geometry_column: geometry,
        },
    })
}

fn copy_bounded(
    reader: &mut impl Read,
    output: &Path,
    limit: u64,
    control: &ImportControl,
) -> MaptileResult<u64> {
    let mut writer = File::options()
        .write(true)
        .create_new(true)
        .open(output)
        .map_err(MaptileError::backend)?;
    let mut written = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        control.check()?;
        let count = reader.read(&mut buffer).map_err(MaptileError::backend)?;
        if count == 0 {
            break;
        }
        written = written
            .checked_add(count as u64)
            .filter(|n| *n <= limit)
            .ok_or_else(|| config("extraction exceeds max_bytes"))?;
        writer
            .write_all(&buffer[..count])
            .map_err(MaptileError::backend)?;
    }
    Ok(written)
}

fn extract_shapefile(
    input: &Path,
    directory: &Path,
    max_bytes: u64,
    control: &ImportControl,
) -> MaptileResult<PathBuf> {
    let mut file = File::open(input).map_err(MaptileError::backend)?;
    check_zip_directory(&mut file)?;
    let mut archive = zip::ZipArchive::with_config(
        zip::read::Config {
            archive_offset: zip::read::ArchiveOffset::Known(0),
        },
        file,
    )
    .map_err(MaptileError::backend)?;
    if archive.len() > 128 || archive.has_duplicate_names() {
        return Err(config("ZIP exceeds 128 entries or contains duplicates"));
    }
    archive
        .validate_header_consistency()
        .map_err(MaptileError::backend)?;
    let mut members = BTreeMap::new();
    let mut expanded = 0u64;
    for i in 0..archive.len() {
        control.check()?;
        let entry = archive.by_index(i).map_err(MaptileError::backend)?;
        let name = entry.name().map_err(MaptileError::backend)?;
        let path = Path::new(name.as_ref());
        if name.contains(['\\', ':', '\0'])
            || path
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
            || entry.enclosed_name().is_none()
            || entry
                .unix_mode()
                .is_some_and(|mode| !matches!(mode & 0o170000, 0 | 0o100000 | 0o040000))
            || entry.encrypted()
            || !matches!(
                entry.compression(),
                zip::CompressionMethod::Stored | zip::CompressionMethod::Deflated
            )
        {
            return Err(config(
                "ZIP contains an unsafe path, nonregular entry, encryption, or unsupported compression",
            ));
        }
        expanded = expanded
            .checked_add(entry.size())
            .filter(|size| *size <= max_bytes)
            .ok_or_else(|| config("ZIP expansion exceeds max_bytes"))?;
        let is_dir = entry.is_dir()
            || entry
                .unix_mode()
                .is_some_and(|mode| mode & 0o170000 == 0o040000);
        let name = name.trim_end_matches('/').to_lowercase();
        if members.insert(name, (i, is_dir)).is_some() {
            return Err(config("ZIP contains duplicate member names"));
        }
    }
    let layers: Vec<_> = members
        .iter()
        .filter_map(|(name, (_, is_dir))| (!is_dir && name.ends_with(".shp")).then_some(name))
        .collect();
    if layers.len() != 1 {
        return Err(config("ZIP must contain exactly one Shapefile"));
    }
    let stem = layers[0]
        .strip_suffix(".shp")
        .expect("checked suffix")
        .to_owned();
    let mut written = 0u64;
    for extension in ["shp", "shx", "dbf", "prj", "cpg"] {
        let Some(&(index, is_dir)) = members.get(&format!("{stem}.{extension}")) else {
            if matches!(extension, "shp" | "shx" | "dbf") {
                return Err(config(format!("Shapefile is missing .{extension}")));
            }
            continue;
        };
        if is_dir {
            return Err(config("Shapefile components must be regular files"));
        }
        let mut entry = archive.by_index(index).map_err(MaptileError::backend)?;
        let size = copy_bounded(
            &mut entry,
            &directory.join(format!("input.{extension}")),
            max_bytes - written,
            control,
        )?;
        if size != entry.size() {
            return Err(config("ZIP member size does not match its header"));
        }
        written += size;
    }
    validate_shapefile_components(directory)?;
    Ok(directory.join("input.shp"))
}

fn validate_shapefile_components(directory: &Path) -> MaptileResult<()> {
    let mut index_records = 0;
    for extension in ["shp", "shx"] {
        let mut file = File::open(directory.join(format!("input.{extension}")))
            .map_err(MaptileError::backend)?;
        let len = file.metadata().map_err(MaptileError::backend)?.len();
        let mut header = [0u8; 100];
        file.read_exact(&mut header)
            .map_err(|_| config(format!("Shapefile .{extension} header is truncated")))?;
        let declared = u64::from(u32::from_be_bytes(
            header[24..28].try_into().expect("fixed header slice"),
        )) * 2;
        if header[..4] != 9994u32.to_be_bytes()
            || header[28..32] != 1000u32.to_le_bytes()
            || declared != len
        {
            return Err(config(format!(
                "Shapefile .{extension} header or declared length is invalid"
            )));
        }
        if extension == "shx" {
            if (len - 100) % 8 != 0 {
                return Err(config("Shapefile .shx has a partial index record"));
            }
            index_records = (len - 100) / 8;
        }
    }
    let mut dbf = File::open(directory.join("input.dbf")).map_err(MaptileError::backend)?;
    let mut header = [0u8; 32];
    dbf.read_exact(&mut header)
        .map_err(|_| config("Shapefile .dbf header is truncated"))?;
    let records = u64::from(u32::from_le_bytes(
        header[4..8].try_into().expect("fixed header slice"),
    ));
    let header_len = u64::from(u16::from_le_bytes([header[8], header[9]]));
    let record_len = u64::from(u16::from_le_bytes([header[10], header[11]]));
    if records != index_records
        || header_len < 33
        || record_len == 0
        || dbf.metadata().map_err(MaptileError::backend)?.len() < header_len + records * record_len
    {
        return Err(config(
            "Shapefile .dbf records are truncated or do not match .shx",
        ));
    }
    Ok(())
}

// Bound metadata allocation before ZipArchive builds its entry index. The
// public ZIP reader has no allocation limit; ZIP64 / split archives are rejected.
fn check_zip_directory(file: &mut File) -> MaptileResult<()> {
    let len = file.metadata().map_err(MaptileError::backend)?.len();
    let tail_len = len.min(65_536 + 22 + 20) as usize;
    file.seek(SeekFrom::End(-(tail_len as i64)))
        .map_err(MaptileError::backend)?;
    let mut tail = vec![0; tail_len];
    file.read_exact(&mut tail).map_err(MaptileError::backend)?;
    let search_start = tail_len.saturating_sub(65_536 + 22);
    let end = (search_start..tail_len.saturating_sub(21))
        .rev()
        .find(|&i| {
            tail[i..].starts_with(b"PK\x05\x06")
                && i + 22 + usize::from(u16::from_le_bytes([tail[i + 20], tail[i + 21]]))
                    <= tail_len
        })
        .ok_or_else(|| config("ZIP has no valid end record"))?;
    let u16_at = |i| u16::from_le_bytes([tail[end + i], tail[end + i + 1]]);
    let u32_at = |i| {
        u32::from_le_bytes([
            tail[end + i],
            tail[end + i + 1],
            tail[end + i + 2],
            tail[end + i + 3],
        ])
    };
    let offset = u64::from(u32_at(16));
    let size = u64::from(u32_at(12));
    let end_offset = len - tail_len as u64 + end as u64;
    let count = u16_at(10);
    if u16_at(4) != 0
        || u16_at(6) != 0
        || u16_at(8) != count
        || !(1..=128).contains(&count)
        || size > 1024 * 1024
        || offset + size != end_offset
        || end + 22 + usize::from(u16_at(20)) != tail_len
        || (end >= 20 && tail[end - 20..].starts_with(b"PK\x06\x07"))
    {
        return Err(config(
            "ZIP requires 1..=128 entries and a 1 MiB directory, without ZIP64, trailing data, or multiple volumes",
        ));
    }
    file.seek(SeekFrom::Start(offset))
        .map_err(MaptileError::backend)?;
    let mut position = offset;
    for _ in 0..count {
        let mut header = [0u8; 46];
        if position + header.len() as u64 > end_offset {
            return Err(config("ZIP directory entry exceeds its bounds"));
        }
        file.read_exact(&mut header)
            .map_err(MaptileError::backend)?;
        if !header.starts_with(b"PK\x01\x02") {
            return Err(config("ZIP directory header is invalid"));
        }
        let variable: u64 = [28, 30, 32]
            .into_iter()
            .map(|i| u64::from(u16::from_le_bytes([header[i], header[i + 1]])))
            .sum();
        position += header.len() as u64 + variable;
        if position > end_offset {
            return Err(config("ZIP directory entry exceeds its bounds"));
        }
        file.seek(SeekFrom::Start(position))
            .map_err(MaptileError::backend)?;
    }
    if position != end_offset {
        return Err(config("ZIP directory entry count does not match its size"));
    }
    file.rewind().map_err(MaptileError::backend)?;
    Ok(())
}

pub(super) struct VectorMetadata {
    pub geometry: String,
    pub srid: i32,
}

pub(super) fn vector_metadata(
    conn: &Connection,
    path: &str,
    driver: &str,
    explicit: Option<i32>,
) -> MaptileResult<VectorMetadata> {
    // Establish the allowed driver before the metadata API (which has no
    // driver-allowlist argument) opens the same private / authorized file.
    conn.prepare(&format!(
        "DESCRIBE SELECT * FROM ST_Read({}, allowed_drivers := [{}], keep_wkb := {})",
        literal(path),
        literal(driver),
        driver != "FlatGeobuf"
    ))
    .map_err(MaptileError::backend)?;
    let (actual_driver, layers, geometries, geometry, authority, code) = conn.query_row(
        &format!("SELECT driver_short_name, array_length(layers), array_length(layers[1].geometry_fields), layers[1].geometry_fields[1].name, layers[1].geometry_fields[1].crs.auth_name, layers[1].geometry_fields[1].crs.auth_code FROM ST_Read_Meta({})", literal(path)),
        [], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<i64>>(2)?, r.get::<_, Option<String>>(3)?, r.get::<_, Option<String>>(4)?, r.get::<_, Option<String>>(5)?)),
    ).map_err(MaptileError::backend)?;
    if actual_driver != driver {
        return Err(config(format!("expected {driver}, found {actual_driver}")));
    }
    if layers != 1 || geometries != Some(1) {
        return Err(config(
            "input must have exactly one layer and geometry field",
        ));
    }
    let geometry = geometry.ok_or_else(|| config("geometry field has no name"))?;
    identifier(&geometry)?;
    let srid = if let Some(srid) = explicit {
        srid
    } else {
        let authority = authority.as_deref().unwrap_or_default();
        let code = code.as_deref().unwrap_or_default();
        if authority.eq_ignore_ascii_case("EPSG") {
            code.parse()
                .map_err(|_| config("CRS has no valid EPSG code; set srid"))?
        } else if authority.eq_ignore_ascii_case("OGC") && code.eq_ignore_ascii_case("CRS84") {
            4326
        } else {
            return Err(config("CRS has no recognized authority; set srid"));
        }
    };
    if srid <= 0 {
        return Err(config("srid must be positive"));
    }
    Ok(VectorMetadata {
        geometry: geometry.to_owned(),
        srid,
    })
}

// Rust's Windows canonicalizer adds a verbatim prefix; expose ordinary absolute
// paths so the same lexical validation can be used when reopening an import.
fn normalized_local_path(path: PathBuf) -> MaptileResult<PathBuf> {
    #[cfg(windows)]
    let path = {
        let text = path_text(&path)?;
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            PathBuf::from(format!(r"\\{rest}"))
        } else if let Some(rest) = text.strip_prefix(r"\\?\") {
            PathBuf::from(rest)
        } else {
            path
        }
    };
    validate_single_file_path(&path)?;
    Ok(path)
}
fn path_text(path: &Path) -> MaptileResult<&str> {
    path.to_str().ok_or_else(|| config("paths must be UTF-8"))
}
fn config(message: impl Into<String>) -> MaptileError {
    MaptileError::Config(format!("local vector: {}", message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::mvt;
    use serde_json::Value;
    use zip::write::SimpleFileOptions;

    const TILE: TileCoord = TileCoord { z: 2, x: 2, y: 1 };
    const POINTS: &str = "SELECT * FROM (VALUES (1::BIGINT, 'inside', ST_Point(10, 60)), (2, 'outside', ST_Point(-100, -50))) t(id, label, geom)";

    fn write_vector(
        dir: &Path,
        filename: &str,
        driver: &str,
        srid: Option<i32>,
        indexed: bool,
        query: &str,
    ) -> PathBuf {
        let conn = spatial_connection(&GeoParquetQueryOptions::default()).unwrap();
        let path = dir.join(filename);
        let srs = srid
            .map(|srid| format!(", SRS 'EPSG:{srid}'"))
            .unwrap_or_default();
        let index = if driver == "FlatGeobuf" {
            format!(
                ", LAYER_CREATION_OPTIONS 'SPATIAL_INDEX={}'",
                if indexed { "YES" } else { "NO" }
            )
        } else {
            String::new()
        };
        conn.execute_batch(&format!(
            "COPY ({query}) TO {} (FORMAT GDAL, DRIVER {}{srs}{index})",
            literal(path.to_str().unwrap()),
            literal(driver)
        ))
        .unwrap();
        path
    }

    fn fgb_config(path: PathBuf) -> FlatGeobufSourceConfig {
        FlatGeobufSourceConfig {
            path,
            layer_name: "places".into(),
            id_column: Some("id".into()),
            properties: Some(vec!["label".into()]),
            ..Default::default()
        }
    }
    fn parquet_config(imported: ImportedGeoParquet) -> GeoParquetSourceConfig {
        GeoParquetSourceConfig {
            path: imported.path,
            geometry_column: Some(imported.geometry_column),
            layer_name: "places".into(),
            id_column: Some("id".into()),
            properties: Some(vec!["label".into()]),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn indexed_flatgeobuf_and_geojson_import_produce_the_same_tile() {
        let dir = tempfile::tempdir().unwrap();
        let fgb = write_vector(
            dir.path(),
            "places's.fgb",
            "FlatGeobuf",
            Some(4326),
            true,
            POINTS,
        );
        let json = write_vector(
            dir.path(),
            "places.geojson",
            "GeoJSON",
            Some(4326),
            false,
            POINTS,
        );
        let source = FlatGeobufSource::open(fgb_config(fgb)).unwrap();
        let imported = import_to_geoparquet(
            json,
            VectorImportFormat::GeoJson,
            dir.path().join("result.parquet"),
            VectorImportOptions::default(),
        )
        .await
        .unwrap();
        let parquet = GeoParquetSource::open(parquet_config(imported)).unwrap();
        let direct = source.get_tile(TILE, None).await.unwrap();
        assert_eq!(
            direct.data,
            parquet.get_tile(TILE, None).await.unwrap().data
        );
        let tile = mvt::decode(&direct.data).unwrap();
        assert_eq!(tile.layers[0].features.len(), 1);
        assert_eq!(tile.layers[0].features[0].id, Some(1));
        assert_eq!(tile.layers[0].name, "places");
        let clone = source.clone();
        source.close().await.unwrap();
        assert!(clone.get_tile(TILE, None).await.is_err());
        parquet.close().await.unwrap();
    }

    #[tokio::test]
    async fn flatgeobuf_keeps_buffer_crossing_and_world_edge_features() {
        let dir = tempfile::tempdir().unwrap();
        let query = "SELECT * FROM (VALUES
            (1::BIGINT, 'buffer', ST_Point(-0.5, 60)),
            (2, 'crossing', ST_GeomFromText('LINESTRING(-10 55, 40 65)')),
            (3, 'polygon', ST_GeomFromText('POLYGON((-10 40,100 40,100 70,-10 70,-10 40),(10 50,10 55,20 55,20 50,10 50))')),
            (4, 'west', ST_Point(-179.5, 60)), (5, 'east', ST_Point(179.5, 60)),
            (6, 'projected chord', ST_GeomFromText('LINESTRING(-170 -80, 170 80)'))
        ) t(id,label,geom)";
        let indexed = write_vector(
            dir.path(),
            "indexed.fgb",
            "FlatGeobuf",
            Some(4326),
            true,
            query,
        );
        let scanned = write_vector(
            dir.path(),
            "scanned.fgb",
            "FlatGeobuf",
            Some(4326),
            false,
            query,
        );
        let indexed = FlatGeobufSource::open(fgb_config(indexed)).unwrap();
        let scanned = FlatGeobufSource::open(fgb_config(scanned)).unwrap();
        for xyz in [
            TILE,
            TileCoord { z: 2, x: 0, y: 1 },
            TileCoord { z: 2, x: 3, y: 1 },
            TileCoord { z: 0, x: 0, y: 0 },
        ] {
            let features = |data: &[u8]| {
                let mut features = mvt::decode(data).unwrap().layers.remove(0).features;
                // Index traversal changes value-table order, not the geometry.
                for feature in &mut features {
                    feature.tags.clear();
                }
                features.sort_by_key(|feature| feature.id);
                features
            };
            let tile = indexed.get_tile(xyz, None).await.unwrap();
            assert_eq!(
                features(&tile.data),
                features(&scanned.get_tile(xyz, None).await.unwrap().data)
            );
            let ids: Vec<_> = features(&tile.data)
                .iter()
                .map(|feature| feature.id.unwrap())
                .collect();
            if xyz == TILE {
                assert!(ids.contains(&1) && ids.contains(&2) && ids.contains(&3));
            }
            if xyz.x == 0 && xyz.z == 2 {
                assert!(ids.contains(&4));
            }
            if xyz.x == 3 {
                assert!(ids.contains(&5));
            }
        }
    }

    #[tokio::test]
    async fn flatgeobuf_validates_crs_driver_limits_and_file_version() {
        let dir = tempfile::tempdir().unwrap();
        let unknown = write_vector(dir.path(), "unknown.fgb", "FlatGeobuf", None, true, POINTS);
        assert!(FlatGeobufSource::open(fgb_config(unknown.clone())).is_err());
        let mut cfg = fgb_config(unknown.clone());
        cfg.srid = Some(4326);
        let source = FlatGeobufSource::open(cfg).unwrap();
        assert!(!source.get_tile(TILE, None).await.unwrap().data.is_empty());
        std::fs::OpenOptions::new()
            .append(true)
            .open(unknown)
            .unwrap()
            .write_all(&[0])
            .unwrap();
        assert!(
            source
                .get_tile(TILE, None)
                .await
                .unwrap_err()
                .to_string()
                .contains("changed")
        );
        let mercator = write_vector(
            dir.path(),
            "mercator.fgb",
            "FlatGeobuf",
            Some(3857),
            true,
            &format!(
                "SELECT id,label,ST_Transform(geom,'EPSG:4326','EPSG:3857',always_xy:=true) AS geom FROM ({POINTS})"
            ),
        );
        let source = FlatGeobufSource::open(fgb_config(mercator.clone())).unwrap();
        assert_eq!(
            mvt::decode(&source.get_tile(TILE, None).await.unwrap().data)
                .unwrap()
                .layers[0]
                .features[0]
                .id,
            Some(1)
        );
        let mut cfg = fgb_config(mercator);
        cfg.query_options.max_tile_bytes = 1;
        assert!(matches!(
            FlatGeobufSource::open(cfg)
                .unwrap()
                .get_tile(TILE, None)
                .await,
            Err(MaptileError::PayloadTooLarge { .. })
        ));
        let json = write_vector(
            dir.path(),
            "wrong.fgb",
            "GeoJSON",
            Some(4326),
            false,
            POINTS,
        );
        assert!(FlatGeobufSource::open(fgb_config(json)).is_err());
        assert!(
            FlatGeobufSource::open(fgb_config(PathBuf::from(
                "https://example.invalid/data.fgb"
            )))
            .is_err()
        );
        assert!(
            source
                .get_tile(TileCoord { z: 2, x: 4, y: 1 }, None)
                .await
                .is_err()
        );
    }

    fn zip_members(path: &Path, members: &[(&str, &[u8])]) {
        let mut writer = zip::ZipWriter::new(File::create(path).unwrap());
        for (name, bytes) in members {
            writer
                .start_file(
                    *name,
                    SimpleFileOptions::default()
                        .compression_method(zip::CompressionMethod::Deflated),
                )
                .unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap();
    }

    fn zip_shape(path: &Path, shp: &Path, include_prj: bool) {
        let mut writer = zip::ZipWriter::new(File::create(path).unwrap());
        writer
            .add_directory("nested/", SimpleFileOptions::default())
            .unwrap();
        for ext in ["shp", "shx", "dbf", "prj"] {
            if ext == "prj" && !include_prj {
                continue;
            }
            writer
                .start_file(
                    format!("nested/places.{}", ext.to_uppercase()),
                    SimpleFileOptions::default(),
                )
                .unwrap();
            writer
                .write_all(&std::fs::read(shp.with_extension(ext)).unwrap())
                .unwrap();
        }
        writer.finish().unwrap();
    }

    #[tokio::test]
    async fn nested_zip_shapefile_import_projects_and_survives_input_removal() {
        let dir = tempfile::tempdir().unwrap();
        let shp = write_vector(
            dir.path(),
            "places.shp",
            "ESRI Shapefile",
            Some(3857),
            false,
            &format!(
                "SELECT id,label,ST_Transform(geom,'EPSG:4326','EPSG:3857',always_xy:=true) AS geom FROM ({POINTS})"
            ),
        );
        let archive = dir.path().join("places.zip");
        zip_shape(&archive, &shp, true);
        let imported = import_to_geoparquet(
            &archive,
            VectorImportFormat::ShapefileZip,
            dir.path().join("result.parquet"),
            VectorImportOptions::default(),
        )
        .await
        .unwrap();
        std::fs::remove_file(&archive).unwrap();
        let source = GeoParquetSource::open(parquet_config(imported)).unwrap();
        let tile = mvt::decode(&source.get_tile(TILE, None).await.unwrap().data).unwrap();
        assert_eq!(tile.layers[0].features.len(), 1);
        assert_eq!(tile.layers[0].features[0].id, Some(1));
        zip_shape(&archive, &shp, false);
        assert!(
            import_to_geoparquet(
                &archive,
                VectorImportFormat::ShapefileZip,
                dir.path().join("missing_crs.parquet"),
                VectorImportOptions::default()
            )
            .await
            .is_err()
        );
        assert!(!dir.path().join("missing_crs.parquet").exists());
        let options = VectorImportOptions {
            srid: Some(3857),
            ..Default::default()
        };
        assert!(
            import_to_geoparquet(
                &archive,
                VectorImportFormat::ShapefileZip,
                dir.path().join("override.parquet"),
                options
            )
            .await
            .is_ok()
        );
    }

    #[tokio::test]
    async fn truncated_shapefile_components_fail_before_publication() {
        let dir = tempfile::tempdir().unwrap();
        let shp = write_vector(
            dir.path(),
            "places.shp",
            "ESRI Shapefile",
            Some(4326),
            false,
            POINTS,
        );
        for extension in ["shp", "shx", "dbf"] {
            let component = shp.with_extension(extension);
            let original = std::fs::read(&component).unwrap();
            std::fs::write(&component, &original[..original.len() - 8]).unwrap();
            let archive = dir.path().join("truncated.zip");
            zip_shape(&archive, &shp, true);
            let output = dir.path().join("result.parquet");
            let error = import_to_geoparquet(
                archive,
                VectorImportFormat::ShapefileZip,
                &output,
                VectorImportOptions::default(),
            )
            .await
            .unwrap_err();
            assert!(error.to_string().contains("Shapefile"));
            assert!(!output.exists());
            std::fs::write(component, original).unwrap();
        }
    }

    #[tokio::test]
    async fn geojson_import_preserves_colliding_attribute_names_and_covering() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("input.geojson");
        std::fs::write(&path, r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"id":7,"geom":"attribute","geometry":"also attribute","__maptile_bbox":9},"geometry":{"type":"Point","coordinates":[10,60]}}]}"#).unwrap();
        let imported = import_to_geoparquet(
            &path,
            VectorImportFormat::GeoJson,
            dir.path().join("result.parquet"),
            VectorImportOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(imported.geometry_column, "geometry_");
        let conn = spatial_connection(&GeoParquetQueryOptions::default()).unwrap();
        let geo: Vec<u8> = conn
            .query_row(
                &format!(
                    "SELECT value FROM parquet_kv_metadata({}) WHERE key='geo'::BLOB",
                    literal(imported.path.to_str().unwrap())
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        let geo: Value = serde_json::from_slice(&geo).unwrap();
        assert_eq!(
            geo["columns"]["geometry_"]["covering"]["bbox"]["xmin"][0],
            "__maptile_bbox_"
        );
        let source = GeoParquetSource::open(GeoParquetSourceConfig {
            path: imported.path,
            geometry_column: Some(imported.geometry_column),
            id_column: Some("id".into()),
            ..Default::default()
        })
        .unwrap();
        let tile = mvt::decode(&source.get_tile(TILE, None).await.unwrap().data).unwrap();
        assert!(tile.layers[0].keys.contains(&"geom".to_owned()));
        assert!(tile.layers[0].keys.contains(&"geometry".to_owned()));
        assert!(tile.layers[0].keys.contains(&"__maptile_bbox".to_owned()));
        assert!(!tile.layers[0].keys.contains(&"__maptile_bbox_".to_owned()));
    }

    #[tokio::test]
    async fn shapefile_import_removes_m_keeps_z_and_uses_null_bbox_for_null_geometry() {
        let dir = tempfile::tempdir().unwrap();
        let shp = write_vector(
            dir.path(),
            "measured.shp",
            "ESRI Shapefile",
            Some(4326),
            false,
            "SELECT * FROM (VALUES (1::BIGINT, ST_GeomFromText('POINT ZM (10 60 12 8)')), (2, NULL)) t(id, geom)",
        );
        let archive = dir.path().join("measured.zip");
        zip_shape(&archive, &shp, true);
        let imported = import_to_geoparquet(
            archive,
            VectorImportFormat::ShapefileZip,
            dir.path().join("result.parquet"),
            VectorImportOptions::default(),
        )
        .await
        .unwrap();
        let conn = spatial_connection(&GeoParquetQueryOptions::default()).unwrap();
        let (has_m, has_z, z): (bool, bool, f64) = conn.query_row(&format!(
            "SELECT ST_HasM(geometry), ST_HasZ(geometry), ST_Z(geometry) FROM read_parquet({}) WHERE id = 1",
            literal(imported.path.to_str().unwrap())), [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).unwrap();
        assert!(!has_m && has_z);
        assert_eq!(z, 12.0);
        let null_bbox: bool = conn.query_row(&format!("SELECT geometry IS NULL AND __maptile_bbox IS NULL FROM read_parquet({}) WHERE id = 2", literal(imported.path.to_str().unwrap())), [], |row| row.get(0)).unwrap();
        assert!(null_bbox);
        let source = GeoParquetSource::open(GeoParquetSourceConfig {
            path: imported.path,
            geometry_column: Some(imported.geometry_column),
            id_column: Some("id".into()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            mvt::decode(&source.get_tile(TILE, None).await.unwrap().data)
                .unwrap()
                .layers[0]
                .features
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn unsupported_driver_column_collisions_fail_without_publishing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("input.geojson");
        let output = dir.path().join("result.parquet");
        std::fs::write(&path, r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"wkb_geometry":"attribute"},"geometry":{"type":"Point","coordinates":[10,60]}}]}"#).unwrap();
        assert!(
            import_to_geoparquet(
                &path,
                VectorImportFormat::GeoJson,
                &output,
                VectorImportOptions::default()
            )
            .await
            .is_err()
        );
        assert!(!output.exists());
    }

    #[test]
    fn zip_validation_rejects_unsafe_ambiguous_and_oversized_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("input.zip");
        let stage = tempfile::tempdir().unwrap();
        for names in [
            vec!["../outside.shp"],
            vec!["/absolute.shp"],
            vec!["C:\\input.shp"],
            vec!["a.shp", "b.shp"],
            vec!["a.shp", "A.SHP"],
            vec!["a.shp", "a.dbf"],
        ] {
            let members: Vec<_> = names
                .iter()
                .map(|name| (*name, b"data".as_slice()))
                .collect();
            zip_members(&archive, &members);
            assert!(
                extract_shapefile(&archive, stage.path(), 1024, &ImportControl::default()).is_err(),
                "accepted {names:?}"
            );
        }
        zip_members(&archive, &[("a.shp", &vec![0u8; 2048])]);
        assert!(
            extract_shapefile(&archive, stage.path(), 1024, &ImportControl::default())
                .unwrap_err()
                .to_string()
                .contains("expansion")
        );
        let mut writer = zip::ZipWriter::new(File::create(&archive).unwrap());
        writer
            .add_symlink("a.shp", "other.shp", SimpleFileOptions::default())
            .unwrap();
        writer.finish().unwrap();
        assert!(
            extract_shapefile(&archive, stage.path(), 1024, &ImportControl::default()).is_err()
        );
        let names: Vec<_> = (0..129).map(|i| format!("item-{i}.txt")).collect();
        let members: Vec<_> = names
            .iter()
            .map(|name| (name.as_str(), b"".as_slice()))
            .collect();
        zip_members(&archive, &members);
        assert!(
            extract_shapefile(&archive, stage.path(), 1024, &ImportControl::default())
                .unwrap_err()
                .to_string()
                .contains("128")
        );
        zip_members(&archive, &[("a.shp", b"data")]);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&archive)
            .unwrap()
            .write_all(b"trailing data")
            .unwrap();
        assert!(
            extract_shapefile(&archive, stage.path(), 1024, &ImportControl::default()).is_err()
        );
        assert!(!dir.path().join("outside.shp").exists());
    }

    #[tokio::test]
    async fn imports_never_replace_or_publish_failed_outputs() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input.geojson");
        let output = dir.path().join("result.parquet");
        std::fs::write(&input, "invalid GeoJSON").unwrap();
        assert!(
            import_to_geoparquet(
                &input,
                VectorImportFormat::GeoJson,
                &output,
                VectorImportOptions::default()
            )
            .await
            .is_err()
        );
        assert!(!output.exists());
        std::fs::write(&output, b"preserve this file").unwrap();
        assert!(
            import_to_geoparquet(
                &input,
                VectorImportFormat::GeoJson,
                &output,
                VectorImportOptions::default()
            )
            .await
            .is_err()
        );
        assert_eq!(std::fs::read(&output).unwrap(), b"preserve this file");
        std::fs::remove_file(&output).unwrap();
        assert!(
            import_to_geoparquet(
                &input,
                VectorImportFormat::GeoJson,
                &output,
                VectorImportOptions {
                    max_bytes: 1,
                    ..Default::default()
                }
            )
            .await
            .is_err()
        );
        assert!(!output.exists());
        for name in ["places[2026].parquet", "places?.parquet", "places*.parquet"] {
            let destination = dir.path().join(name);
            let error = import_to_geoparquet(
                &input,
                VectorImportFormat::GeoJson,
                &destination,
                VectorImportOptions::default(),
            )
            .await
            .unwrap_err();
            assert!(error.to_string().contains("local single file"));
            assert!(!destination.exists());
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn import_deadline_discards_staging_without_publication() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("input.geojson");
        std::fs::write(
            &path,
            format!(
                "{{\"type\":\"FeatureCollection\",\"features\":[],\"padding\":\"{}\"}}",
                "x".repeat(8 * 1024 * 1024)
            ),
        )
        .unwrap();
        let output = dir.path().join("result.parquet");
        assert!(
            import_to_geoparquet(
                &path,
                VectorImportFormat::GeoJson,
                &output,
                VectorImportOptions {
                    timeout_ms: 1,
                    ..Default::default()
                }
            )
            .await
            .is_err()
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            while std::fs::read_dir(dir.path()).unwrap().count() != 1 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(!output.exists());
    }
}
