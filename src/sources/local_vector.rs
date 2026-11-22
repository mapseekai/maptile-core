//! Local FlatGeobuf tiles and one-shot conversion to GeoParquet.
//!
//! Requires `duckdb` and the same signed DuckDB Spatial extension as
//! GeoParquet. No system GDAL, remote URLs, user SQL, or driver options are used.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use duckdb::Connection;
use tempfile::TempDir;

use super::duckdb_query::{FinishQuery, QueryControl};
use super::file_stamp::FileStamp;
use super::geoparquet::{
    BoundsMode, DuckDbPoolStatus, GeoParquetQueryOptions, GeoParquetSource, GeoParquetSourceConfig,
    validate_config,
};
use super::geoparquet_geometry::{
    GeometrySource, identifier, literal, load_spatial, normalized_local_path, single_file_path,
    spatial_database, unused_name, validate_single_file_path,
};
use super::{SourceKind, TileCoord, TileInfo, TileQuery, TileResponse, TileSource};
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
    pub bounds_mode: BoundsMode,
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
            bounds_mode: BoundsMode::Skip,
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
            path: single_file_path(&cfg.path)?,
            minzoom: cfg.minzoom,
            maxzoom: cfg.maxzoom,
            bounds: cfg.bounds,
            name: cfg.name,
            attribution: cfg.attribution,
            description: cfg.description,
            geometry_column: Some("geom".into()),
            layer_name: cfg.layer_name,
            bounds_mode: cfg.bounds_mode,
            srid: cfg.srid,
            id_column: cfg.id_column,
            properties: cfg.properties,
            extent: cfg.extent,
            buffer: cfg.buffer,
            query_options: cfg.query_options,
            cache_zoom: cfg.cache_zoom,
            ..Default::default()
        };
        validate_config(&cfg)?;
        let stamp = FileStamp::read(&cfg.path)?;
        let geometry = GeometrySource::open_flatgeobuf(&cfg)?;
        if !matches!(FileStamp::read(&cfg.path), Ok(current) if current == stamp) {
            return Err(MaptileError::VersionChanged("local FlatGeobuf file".into()));
        }
        #[cfg(any(unix, windows))]
        let version = crate::cache::ContentVersion::from_input(
            format!("flatgeobuf:{cfg:?}:{stamp:?}").as_bytes(),
        );
        #[cfg(not(any(unix, windows)))]
        let version = crate::cache::ContentVersion::new_generation();
        Ok(Self(GeoParquetSource::from_opened(cfg, geometry, version)))
    }

    /// Retain ownership of the source file through clones and active queries.
    pub fn with_file_lease(self, lease: impl Send + Sync + 'static) -> Self {
        Self(self.0.with_file_lease(lease))
    }

    /// Snapshot of query admission, live work, and managed connections.
    pub fn pool_status(&self) -> DuckDbPoolStatus {
        self.0.pool_status()
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
    fn source_kind(&self) -> SourceKind {
        SourceKind::FlatGeobuf
    }
    fn strict_mvt(&self) -> bool {
        true
    }
    fn consistent_export(&self) -> bool {
        true
    }
    fn supports_filter(&self) -> bool {
        true
    }
    fn supports_bbox(&self) -> bool {
        true
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

/// Import limits. Each import uses two DuckDB threads, 256 MB of DuckDB-managed memory,
/// and no disk spilling. Applications must bound the number of concurrent imports.
#[derive(Debug, Clone)]
pub struct VectorImportOptions {
    /// Override the input CRS with a positive EPSG code.
    pub srid: Option<i32>,
    /// Maximum input and completed output bytes, each separately (default 512 MiB).
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

/// Import a local FlatGeobuf file once, then serve its GeoParquet.
///
/// The output contains WGS84 WKB (M removed, Z retained), original attributes, and a GeoParquet 1.1 bbox
/// covering. Geometry / bbox names are chosen to avoid attribute collisions.
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
    let control = QueryControl::pending();
    let _cancel = control.watch(deadline);
    let task = tokio::task::spawn_blocking(move || {
        let _finish = FinishQuery(control.clone());
        import_staged(&input, &output, &options, &control)
    });
    let staged = tokio::time::timeout_at(deadline, task)
        .await
        .map_err(|_| MaptileError::Deadline)?
        .map_err(MaptileError::backend)??;
    // Publication stays outside the worker: a dropped future cannot publish.
    if tokio::time::Instant::now() >= deadline {
        return Err(MaptileError::Deadline);
    }
    std::fs::hard_link(
        staged.dir.path().join("result.parquet"),
        &staged.result.path,
    )
    .map_err(MaptileError::backend)?;
    Ok(staged.result)
}

fn check_cancelled(control: &QueryControl) -> MaptileResult<()> {
    if control.cancelled() {
        Err(MaptileError::Cancelled)
    } else {
        Ok(())
    }
}

struct StagedImport {
    dir: TempDir,
    result: ImportedGeoParquet,
}

fn import_staged(
    input: &Path,
    output: &Path,
    options: &VectorImportOptions,
    control: &Arc<QueryControl>,
) -> MaptileResult<StagedImport> {
    check_cancelled(control)?;
    let input = single_file_path(input)?;
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
    let source = dir.path().join("input.fgb");
    copy_bounded(
        &mut File::open(&input).map_err(MaptileError::backend)?,
        &source,
        options.max_bytes,
        control,
    )?;
    if FileStamp::read(&input)? != stamp {
        return Err(config("input changed during staging"));
    }
    check_cancelled(control)?;
    let conn = spatial_database(&GeoParquetQueryOptions {
        pool_size: 1,
        ..Default::default()
    })?;
    let _binding = control.bind(conn.interrupt_handle(), None);
    check_cancelled(control)?;
    load_spatial(&conn)?;
    let allowed = path_text(dir.path())?;
    conn.execute_batch(&format!(
        "SET allowed_directories = [{}]; SET enable_external_access = false;",
        literal(&format!("{allowed}/"))
    ))
    .map_err(MaptileError::backend)?;
    let source = path_text(&source)?;
    let metadata = vector_metadata(&conn, source, options.srid)?;
    let from = format!(
        "ST_Read({}, allowed_drivers := ['FlatGeobuf'])",
        literal(source)
    );
    let columns: BTreeMap<String, String> = conn
        .prepare(&format!("DESCRIBE SELECT * FROM {from}"))
        .and_then(|mut s| {
            s.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<_, _>>()
        })
        .map_err(MaptileError::backend)?;
    if !columns
        .get(&metadata.geometry)
        .is_some_and(|ty| ty.starts_with("GEOMETRY"))
    {
        return Err(config("FlatGeobuf did not provide a geometry column"));
    }
    let geometry = unused_name("geometry", &columns);
    let bbox = unused_name("__maptile_bbox", &columns);
    let geom = identifier(&geometry)?;
    let bbox_sql = identifier(&bbox)?;
    let properties = columns
        .keys()
        .filter(|name| **name != metadata.geometry)
        .map(|name| identifier(name).map(|name| format!(", {name}")))
        .collect::<MaptileResult<String>>()?;
    let raw = identifier(&metadata.geometry)?;
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
    check_cancelled(control)?;
    conn.execute_batch(&format!(
        "COPY (SELECT ST_AsWKB({geom})::BLOB AS {geom}{properties}, CASE WHEN {geom} IS NOT NULL THEN struct_pack(xmin := ST_XMin({geom}), ymin := ST_YMin({geom}), xmax := ST_XMax({geom}), ymax := ST_YMax({geom})) END AS {bbox_sql} FROM (SELECT {projected} AS {geom}{properties} FROM {from})) TO {} (FORMAT PARQUET, KV_METADATA {{geo: {}}})",
        literal(path_text(&result_path)?), literal(&geo.to_string())
    )).map_err(MaptileError::backend)?;
    check_cancelled(control)?;
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
    control: &Arc<QueryControl>,
) -> MaptileResult<u64> {
    let mut writer = File::options()
        .write(true)
        .create_new(true)
        .open(output)
        .map_err(MaptileError::backend)?;
    let mut written = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        check_cancelled(control)?;
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

pub(super) struct VectorMetadata {
    pub geometry: String,
    pub srid: i32,
}

pub(super) fn vector_metadata(
    conn: &Connection,
    path: &str,
    explicit: Option<i32>,
) -> MaptileResult<VectorMetadata> {
    // Establish the allowed driver before the metadata API (which has no
    // driver-allowlist argument) opens the same private / authorized file.
    conn.prepare(&format!(
        "DESCRIBE SELECT * FROM ST_Read({}, allowed_drivers := ['FlatGeobuf'])",
        literal(path)
    ))
    .map_err(MaptileError::backend)?;
    let (actual_driver, layers, geometries, geometry, authority, code) = conn.query_row(
        &format!("SELECT driver_short_name, array_length(layers), array_length(layers[1].geometry_fields), layers[1].geometry_fields[1].name, layers[1].geometry_fields[1].crs.auth_name, layers[1].geometry_fields[1].crs.auth_code FROM ST_Read_Meta({})", literal(path)),
        [], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<i64>>(2)?, r.get::<_, Option<String>>(3)?, r.get::<_, Option<String>>(4)?, r.get::<_, Option<String>>(5)?)),
    ).map_err(MaptileError::backend)?;
    if actual_driver != "FlatGeobuf" {
        return Err(config("expected a FlatGeobuf file"));
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

fn path_text(path: &Path) -> MaptileResult<&str> {
    path.to_str().ok_or_else(|| config("paths must be UTF-8"))
}
fn config(message: impl Into<String>) -> MaptileError {
    MaptileError::Config(format!("local vector: {}", message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::geoparquet_geometry::spatial_connection;
    use crate::sources::mvt;

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
    async fn indexed_flatgeobuf_import_produces_the_same_tile() {
        let dir = tempfile::tempdir().unwrap();
        let fgb = write_vector(
            dir.path(),
            "places's.fgb",
            "FlatGeobuf",
            Some(4326),
            true,
            POINTS,
        );
        let source = FlatGeobufSource::open(fgb_config(fgb.clone())).unwrap();
        let imported = import_to_geoparquet(
            fgb,
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
        let filter = crate::filter::parse_cql2("label = 'inside'").unwrap();
        let query = TileQuery {
            bbox: Some([9.0, 59.0, 11.0, 61.0]),
            filter: Some(&filter),
            ..Default::default()
        };
        assert_eq!(
            direct.data,
            source.get_tile(TILE, Some(&query)).await.unwrap().data
        );
        let clone = source.clone();
        source.close().await.unwrap();
        assert!(clone.get_tile(TILE, None).await.is_err());
        parquet.close().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn flatgeobuf_atomic_replacement_changes_manager_cache_generation() {
        use crate::cache::TileCache;
        use crate::sources::manager::SourceManager;
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().unwrap();
        let replacement_dir = tempfile::tempdir().unwrap();
        let path = write_vector(
            dir.path(),
            "places.fgb",
            "FlatGeobuf",
            Some(4326),
            true,
            POINTS,
        );
        let replacement = write_vector(
            replacement_dir.path(),
            "places.fgb",
            "FlatGeobuf",
            Some(4326),
            true,
            &POINTS.replace("ST_Point(10, 60)", "ST_Point(11, 60)"),
        );
        let before = std::fs::metadata(&path).unwrap();
        File::options()
            .write(true)
            .open(&replacement)
            .unwrap()
            .set_modified(before.modified().unwrap())
            .unwrap();
        let after = std::fs::metadata(&replacement).unwrap();
        assert_eq!(before.len(), after.len());
        assert_eq!(before.modified().unwrap(), after.modified().unwrap());
        assert_ne!(before.ino(), after.ino());

        let cfg = fgb_config(path.clone());
        let old = Arc::new(FlatGeobufSource::open(cfg.clone()).unwrap());
        let manager =
            SourceManager::new().with_cache(Arc::new(TileCache::new(8, None, None).unwrap()));
        manager.register(old.clone());
        let cached = manager.get_tile(&cfg.id, TILE, None).await.unwrap().data;
        std::fs::rename(replacement, &path).unwrap();
        assert!(old.get_tile(TILE, None).await.is_err());
        let new = Arc::new(FlatGeobufSource::open(cfg.clone()).unwrap());
        assert_ne!(old.content_version(), new.content_version());
        assert_eq!(new.content_version(), new.clone_source().content_version());
        let expected = new.get_tile(TILE, None).await.unwrap().data;
        assert_ne!(cached, expected);
        manager.register(new.clone());
        assert_eq!(
            manager.get_tile(&cfg.id, TILE, None).await.unwrap().data,
            expected
        );
        old.close().await.unwrap();
        new.close().await.unwrap();
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

    #[tokio::test]
    async fn import_preserves_existing_destination() {
        let dir = tempfile::tempdir().unwrap();
        let input = write_vector(
            dir.path(),
            "input.fgb",
            "FlatGeobuf",
            Some(4326),
            true,
            POINTS,
        );
        let output = dir.path().join("output.parquet");
        std::fs::write(&output, b"existing").unwrap();
        assert!(
            import_to_geoparquet(input, &output, VectorImportOptions::default())
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(&output).unwrap(), b"existing");
    }
}
