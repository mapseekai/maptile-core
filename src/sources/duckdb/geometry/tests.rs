use super::super::pool::spatial_connection;
use super::super::{identifier, literal};
use super::bounds::RADIUS;
use super::resolver::resolve_srid;
use super::*;
use crate::sources::mvt;
use crate::sources::{GeoParquetSource, TileSource};
use arrow_array::{
    ArrayRef, BinaryArray, Float64Array, Int64Array, RecordBatch, StringArray, StructArray,
    UInt64Array,
};
use arrow_schema::{DataType, Field, Schema};
use duckdb::named_params;
use parquet::arrow::ArrowWriter;
use parquet::file::metadata::KeyValue;
use parquet::file::properties::WriterProperties;
use serde_json::Value;
use std::sync::Mutex;
use tokio::sync::{Notify, Semaphore};

const TILE: TileCoord = TileCoord { z: 2, x: 2, y: 1 };

#[tokio::test]
async fn flatgeobuf_scan_keeps_a_projected_chord_and_empty_tiles() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chord.fgb");
    let conn = spatial_connection(&GeoParquetQueryOptions::default()).unwrap();
    conn.execute_batch(&format!(
        "COPY (SELECT 6::BIGINT AS id, ST_GeomFromText('LINESTRING(-170 -80,170 80)') AS geom) TO {} (FORMAT GDAL, DRIVER 'FlatGeobuf', SRS 'EPSG:4326', LAYER_CREATION_OPTIONS 'SPATIAL_INDEX=YES')",
        literal(path.to_str().unwrap())
    )).unwrap();
    let cfg = GeoParquetSourceConfig {
        path,
        geometry_column: Some("geom".into()),
        id_column: Some("id".into()),
        properties: Some(vec![]),
        ..Default::default()
    };
    let source = GeometrySource::open_flatgeobuf(&cfg).unwrap();
    let plan: String = conn
        .prepare(&format!("EXPLAIN {}", source.query.sql))
        .unwrap()
        .query_row(named_params! { "z": 4, "x": 9, "y": 6 }, |row| row.get(1))
        .unwrap();
    assert!(
        plan.contains("ST_READ") && !plan.contains("Filter (MinX, MinY, MaxX"),
        "{plan}"
    );
    let tile = source
        .tile(TileCoord { z: 4, x: 9, y: 6 }, None)
        .await
        .unwrap();
    let tile = mvt::decode(&tile).unwrap();
    assert_eq!(tile.layers[0].features.len(), 1);
    assert_eq!(tile.layers[0].features[0].id, Some(6));
    let empty = source
        .tile(TileCoord { z: 4, x: 0, y: 0 }, None)
        .await
        .unwrap();
    assert!(
        mvt::decode(&empty)
            .unwrap()
            .layers
            .iter()
            .all(|layer| layer.features.is_empty())
    );
    source.close().await.unwrap();
}

#[tokio::test]
async fn genuine_geoparquet_projects_filters_and_preserves_properties_and_ids() {
    let fixture = fixture(
        vec![point(10.0, 60.0), point(-100.0, -50.0), None],
        None,
        true,
    );
    let source = GeoParquetSource::open(fixture.config()).unwrap();
    let bytes = source.get_tile(TILE, None).await.unwrap().data;
    let tile = mvt::decode(&bytes).unwrap();
    let layer = &tile.layers[0];
    assert_eq!(layer.name, "places");
    assert_eq!(layer.extent, Some(4096));
    assert_eq!(layer.features.len(), 1);
    let feature = &layer.features[0];
    assert_eq!(feature.id, Some(1));
    assert_eq!(feature.geometry[0], 9);
    let (x, y) = (unzig(feature.geometry[1]), unzig(feature.geometry[2]));
    assert!((x - 455).abs() <= 1);
    assert!(
        (y - 662).abs() <= 1,
        "latitude 60 must be projected, not linearly scaled: {y}"
    );
    let props = properties(layer, feature);
    assert_eq!(props["name"].string_value.as_deref(), Some("row-0"));
    assert_eq!(
        props["unsigned"].string_value.as_deref(),
        Some("18446744073709551615")
    );
    assert_eq!(
        source.tilejson().vector_layers.as_ref().unwrap()[0].fields["unsigned"],
        "String"
    );
    assert!(!layer.keys.iter().any(|k| k == "bbox" || k == "id"));
}

#[tokio::test]
async fn mercator_source_matches_wgs84_and_unknown_crs_requires_override() {
    let projected = fixture(
        vec![point(
            10f64.to_radians() * RADIUS,
            (std::f64::consts::FRAC_PI_4 + 60f64.to_radians() / 2.0)
                .tan()
                .ln()
                * RADIUS,
        )],
        Some(Value::Null),
        true,
    );
    assert!(GeoParquetSource::open(projected.config()).is_err());
    let mut cfg = projected.config();
    cfg.srid = Some(3857);
    let source = GeoParquetSource::open(cfg).unwrap();
    let tile = mvt::decode(&source.get_tile(TILE, None).await.unwrap().data).unwrap();
    let feature = &tile.layers[0].features[0];
    assert!((unzig(feature.geometry[1]) - 455).abs() <= 1);
    assert!((unzig(feature.geometry[2]) - 662).abs() <= 1);
    assert_eq!(
        resolve_srid(
            None,
            Some(&serde_json::json!({"crs":{"id":{"authority":"EPSG","code":3857}}}))
        )
        .unwrap(),
        3857
    );
    assert!(resolve_srid(Some(-1), None).is_err());
    assert!(
        resolve_srid(
            None,
            Some(&serde_json::json!({"crs":{"id":{"authority":"UNKNOWN","code":42}}}))
        )
        .is_err()
    );
}

#[tokio::test]
async fn buffer_edge_world_edge_and_crossing_shapes_are_not_pruned() {
    let fixture = fixture(
        vec![
            point(-0.5, 60.0),
            line(&[(-10.0, 55.0), (40.0, 65.0)]),
            polygon(),
        ],
        None,
        true,
    );
    let source = GeoParquetSource::open(fixture.config()).unwrap();
    let tile = mvt::decode(&source.get_tile(TILE, None).await.unwrap().data).unwrap();
    let layer = &tile.layers[0];
    assert_eq!(
        layer.features.len(),
        3,
        "overlap, not containment, must drive bbox pruning"
    );
    assert!(
        unzig(layer.features[0].geometry[1]) < 0,
        "buffer point retained outside nominal tile"
    );
    let polygon = layer.features.iter().find(|f| f.r#type == Some(3)).unwrap();
    assert_eq!(
        polygon.geometry.iter().filter(|v| **v == 15).count(),
        2,
        "outer ring and hole"
    );
    for feature in &layer.features {
        for (x, y) in coordinates(&feature.geometry) {
            assert!((-64..=4160).contains(&x));
            assert!((-64..=4160).contains(&y));
        }
    }
    let edge = fixture_points(&[(-179.5, 60.0), (179.5, 60.0)], true);
    let source = GeoParquetSource::open(edge.config()).unwrap();
    for x in [0, 3] {
        let tile = mvt::decode(
            &source
                .get_tile(TileCoord { z: 2, x, y: 1 }, None)
                .await
                .unwrap()
                .data,
        )
        .unwrap();
        assert_eq!(
            tile.layers[0].features.len(),
            1,
            "world edge buffer must not invert the envelope"
        );
    }
}

#[tokio::test]
async fn empty_files_null_geometry_and_invalid_coordinates_are_explicit() {
    for data in [vec![], vec![None]] {
        let fixture = fixture(data, None, true);
        let src = GeoParquetSource::open(fixture.config()).unwrap();
        let bytes = src.get_tile(TILE, None).await.unwrap().data;
        assert!(
            bytes.is_empty()
                || mvt::decode(&bytes)
                    .unwrap()
                    .layers
                    .iter()
                    .all(|l| l.features.is_empty())
        );
        for xyz in [
            TileCoord { z: 31, x: 0, y: 0 },
            TileCoord { z: 2, x: 4, y: 0 },
            TileCoord { z: 2, x: 0, y: 4 },
        ] {
            assert!(matches!(
                src.get_tile(xyz, None).await,
                Err(MaptileError::InvalidRequest(_))
            ));
        }
    }
}

#[tokio::test]
async fn identifiers_paths_properties_and_limits_are_safe() {
    let f = fixture_points(&[(10.0, 60.0), (20.0, 60.0)], true);
    let mut cfg = f.config();
    cfg.layer_name = "layer'); DROP TABLE anything; --".into();
    cfg.properties = Some(vec!["name".into()]);
    let src = GeoParquetSource::open(cfg).unwrap();
    let tile = mvt::decode(&src.get_tile(TILE, None).await.unwrap().data).unwrap();
    assert_eq!(tile.layers[0].name, "layer'); DROP TABLE anything; --");
    assert_eq!(tile.layers[0].keys, vec!["name"]);
    let mut cfg = f.config();
    cfg.query_options.max_features = 1;
    assert!(matches!(
        GeoParquetSource::open(cfg)
            .unwrap()
            .get_tile(TILE, None)
            .await,
        Err(MaptileError::Resource(_))
    ));
    let mut cfg = f.config();
    cfg.query_options.max_tile_bytes = 1;
    assert!(
        GeoParquetSource::open(cfg)
            .unwrap()
            .get_tile(TILE, None)
            .await
            .is_err()
    );
    for bad in [
        "/tmp/*.parquet",
        "/tmp/x?.parquet",
        "https://example.com/data.parquet",
    ] {
        assert!(single_file_path(Path::new(bad)).is_err());
    }
    assert_eq!(identifier("a\"; --").unwrap(), "\"a\"\"; --\"");
    for mutate in [0, 1, 2, 3] {
        let mut cfg = f.config();
        match mutate {
            0 => cfg.srid = Some(999_999),
            1 => cfg.id_column = Some("name".into()),
            2 => cfg.properties = Some(vec!["not there".into()]),
            _ => cfg.geometry_column = Some("wrong\"; SELECT".into()),
        };
        assert!(GeoParquetSource::open(cfg).is_err());
    }
}

#[tokio::test]
async fn pruning_reaches_parquet_statistics_and_does_not_change_tiles() {
    let f = fixture_points(&[(10.0, 60.0), (20.0, 60.0)], true);
    let with = GeometrySource::open(&f.config()).unwrap();
    let without_fixture = fixture_points(&[(10.0, 60.0), (20.0, 60.0)], false);
    let without = GeometrySource::open(&without_fixture.config()).unwrap();
    let tile = with.tile(TILE, None).await.unwrap();
    assert_eq!(tile, without.tile(TILE, None).await.unwrap());
    let conn = spatial_connection(&f.config().query_options).unwrap();
    let explain = |source: &GeometrySource| {
        conn.prepare(&format!("EXPLAIN {}", source.query.sql))
            .unwrap()
            .query_row(named_params! {"z":2,"x":1,"y":3}, |r| r.get::<_, String>(1))
            .unwrap()
    };
    let optimized = explain(&with);
    let control = explain(&without);
    assert!(
        optimized.contains("EMPTY_RESULT"),
        "no statistics pruning: {optimized}"
    );
    assert!(
        control.contains("READ_PARQUET"),
        "control must scan: {control}"
    );
}

#[tokio::test]
async fn polar_buffer_matches_exact_geometry() {
    let fixture = fixture_points(&[(30.0, 85.1), (30.0, -85.1)], true);
    let covering = GeoParquetSource::open(fixture.config()).unwrap();
    let mut config = fixture.config();
    config.trust_covering = false;
    let exact = GeoParquetSource::open(config).unwrap();
    for (y, point) in [(0, (1365, -26)), (3, (1365, 4122))] {
        let xyz = TileCoord { z: 2, x: 2, y };
        let data = covering.get_tile(xyz, None).await.unwrap().data;
        assert_eq!(data, exact.get_tile(xyz, None).await.unwrap().data);
        let tile = mvt::decode(&data).unwrap();
        let layer = &tile.layers[0];
        assert_eq!(layer.features.len(), 1);
        assert_eq!(coordinates(&layer.features[0].geometry), vec![point]);
    }
}

#[tokio::test]
async fn bbox_and_cql2_parameters_filter_geometry_before_encoding() {
    let fixture = fixture_points(&[(10.0, 60.0), (20.0, 62.0)], true);
    let source = GeoParquetSource::open(fixture.config()).unwrap();
    let filter =
        crate::filter::parse_cql2("name IN ('row-0', 'row-1') AND NOT (name IS NULL)").unwrap();
    let query = TileQuery {
        filter: Some(&filter),
        bbox: Some([15.0, 55.0, 25.0, 65.0]),
        ..Default::default()
    };
    let data = source.get_tile(TILE, Some(&query)).await.unwrap().data;
    let tile = mvt::decode(&data).unwrap();
    assert_eq!(tile.layers[0].features.len(), 1);
    assert_eq!(tile.layers[0].features[0].id, Some(2));
    let unknown = crate::filter::parse_cql2("private = 'p2'").unwrap();
    let query = TileQuery {
        filter: Some(&unknown),
        ..Default::default()
    };
    assert!(source.get_tile(TILE, Some(&query)).await.is_err());
}

#[tokio::test]
async fn cached_tile_queries_rebind_filters_before_the_feature_limit() {
    let fixture = fixture_points(&[(10.0, 60.0), (15.0, 61.0), (20.0, 62.0)], true);
    let mut cfg = fixture.config();
    cfg.query_options.pool_size = 1;
    cfg.query_options.max_features = 1;
    let source = GeoParquetSource::open(cfg).unwrap();

    for (expression, id) in [("name = 'row-2'", 3), ("name = 'row-0'", 1)] {
        let filter = crate::filter::parse_cql2(expression).unwrap();
        let query = TileQuery {
            filter: Some(&filter),
            ..Default::default()
        };
        let data = source.get_tile(TILE, Some(&query)).await.unwrap().data;
        let tile = mvt::decode(&data).unwrap();
        let features = &tile.layers[0].features;
        assert_eq!(features.len(), 1);
        assert_eq!(features[0].id, Some(id));
    }

    for query in [None, Some(TileQuery::default())] {
        assert!(matches!(
            source.get_tile(TILE, query.as_ref()).await,
            Err(MaptileError::Resource(_))
        ));
    }
}

#[tokio::test]
async fn opening_uses_primary_column_and_calculates_point_bounds() {
    let fixture = fixture_points(&[(10.0, 60.0)], false);
    let mut config = fixture.config();
    config.geometry_column = None;
    config.bounds_mode = BoundsMode::Calc;
    let source = GeoParquetSource::open(config).unwrap();
    let bounds = source.tilejson().bounds.unwrap();
    assert!(bounds.left < 10.0 && bounds.right > 10.0);
    assert!(bounds.bottom < 60.0 && bounds.top > 60.0);
    assert!(!source.get_tile(TILE, None).await.unwrap().data.is_empty());
}

#[tokio::test]
async fn changed_file_is_not_served_with_stale_metadata() {
    let f = fixture_points(&[(10.0, 60.0)], true);
    let src = GeoParquetSource::open(f.config()).unwrap();
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&f.path)
        .unwrap()
        .write_all(b"changed")
        .unwrap();
    assert!(
        src.get_tile(TILE, None)
            .await
            .unwrap_err()
            .to_string()
            .contains("changed")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeouts_and_cancellation_keep_pool_bounded_and_reusable() {
    let f = fixture_points(&[(10.0, 60.0)], true);
    let mut cfg = f.config();
    cfg.query_options.pool_size = 1;
    let mut source = GeometrySource::open(&cfg).unwrap();
    assert!(!source.tile(TILE, None).await.unwrap().is_empty());
    source.options.timeout_ms = 50;
    let normal = source.query.sql.clone();
    source.query.sql="SELECT NULL::BLOB, sum(i)::BIGINT FROM range(1000000000) t(i) WHERE i % ($z::INTEGER + 2) = $x::INTEGER AND i > $y::INTEGER".into();
    let source = Arc::new(source);
    assert!(source.tile(TILE, None).await.is_err());
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if source.pool.slots.available_permits() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let cloned = source.clone();
    let task = tokio::spawn(async move { cloned.tile(TILE, None).await });
    tokio::time::sleep(Duration::from_millis(10)).await;
    task.abort();
    let _ = task.await;
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if source.pool.slots.available_permits() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let mut source = Arc::try_unwrap(source).ok().unwrap();
    source.query.sql = normal;
    source.options.timeout_ms = 10_000;
    for _ in 0..5 {
        assert!(!source.tile(TILE, None).await.unwrap().is_empty());
    }
    let permit = source.pool.slots.clone().acquire_owned().await.unwrap();
    source.options.timeout_ms = 20;
    assert!(source.tile(TILE, None).await.is_err());
    assert_eq!(source.pool.slots.available_permits(), 0);
    drop(permit);
    assert_eq!(source.pool.slots.available_permits(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clones_share_pool_and_file_lease_until_last_reader() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Lease(Arc<AtomicUsize>);
    impl Drop for Lease {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let f = fixture_points(&[(10.0, 60.0)], true);
    let dropped = Arc::new(AtomicUsize::new(0));
    let src = GeoParquetSource::open(f.config())
        .unwrap()
        .with_file_lease(Lease(dropped.clone()));
    let cloned = src.clone_source();
    drop(src);
    assert_eq!(dropped.load(Ordering::SeqCst), 0);
    assert!(!cloned.get_tile(TILE, None).await.unwrap().data.is_empty());
    drop(cloned);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[test]
fn query_connections_cannot_read_another_file() {
    let allowed = fixture_points(&[(10.0, 60.0)], true);
    let other = fixture_points(&[(20.0, 60.0)], true);
    let manager = SpatialManager::new(
        allowed.config().query_options,
        allowed.path.to_str().unwrap(),
        None,
    );
    let conn = manager.open(&QueryControl::pending(), None).unwrap();
    {
        let err = conn
            .query_row(
                &format!(
                    "SELECT count(*) FROM read_parquet({})",
                    literal(other.path.to_str().unwrap())
                ),
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("Permission") || message.contains("disabled"),
            "unexpected failure: {message}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_query_retains_then_releases_its_file_lease() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Lease(Arc<AtomicUsize>);
    impl Drop for Lease {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let fixture = fixture_points(&[(10.0, 60.0)], true);
    let mut source = GeometrySource::open(&fixture.config()).unwrap();
    source.query.sql = "SELECT NULL::BLOB, sum(i)::BIGINT FROM range(1000000000) t(i) WHERE i % ($z::INTEGER + 2) = $x::INTEGER AND i > $y::INTEGER".into();
    let pool = source.pool.clone();
    let dropped = Arc::new(AtomicUsize::new(0));
    let lease = Arc::new(Lease(dropped.clone()));
    let task = tokio::spawn(async move { source.tile(TILE, Some(lease)).await });
    tokio::time::timeout(Duration::from_secs(1), async {
        while pool.slots.available_permits() == 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    task.abort();
    let _ = task.await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while dropped.load(Ordering::SeqCst) == 0 || pool.slots.available_permits() != 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test]
#[ignore = "explicit small performance probe, not a production benchmark"]
async fn repeated_tile_probe() {
    let f = fixture_points(
        &(0..100_000)
            .map(|i| (10.0 + f64::from(i % 100) / 100.0, 60.0))
            .collect::<Vec<_>>(),
        true,
    );
    let start = std::time::Instant::now();
    let source = GeoParquetSource::open(f.config()).unwrap();
    let opening = start.elapsed();
    let start = std::time::Instant::now();
    for _ in 0..20 {
        source
            .get_tile(
                TileCoord {
                    z: 12,
                    x: 2161,
                    y: 1189,
                },
                None,
            )
            .await
            .unwrap();
    }
    eprintln!(
        "100k features, open={opening:?}, 20 uncached tiles={:?}",
        start.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_range_open_tile_and_close_do_not_materialize_the_object() {
    let points: Vec<_> = (0..100_000)
        .map(|i| {
            if i == 0 {
                (10.0, 60.0)
            } else {
                (-100.0, -50.0)
            }
        })
        .collect();
    let fixture = fixture_points(&points, true);
    let http = TestHttp::start(std::fs::read(&fixture.path).unwrap()).await;
    let original_url = format!("{}?signature=opaque", http.url);
    let remote = RemoteLocation::http(&original_url, BTreeMap::new()).unwrap();
    let cfg = fixture.config();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let running = QueryControl::pending();
    let cancellation = running.watch(deadline);
    let source = tokio::task::spawn_blocking(move || {
        let _completion = FinishQuery(running.clone());
        GeometrySource::open_remote(&cfg, Arc::new(remote), &running, None)
    })
    .await
    .unwrap()
    .unwrap();
    drop(cancellation);
    assert!(
        source.query.sql.contains(&original_url),
        "DuckDB must query the original URL"
    );
    source
        .pool
        .run(deadline, None, |conn| {
            assert!(
                conn.query_row(
                    "SELECT current_setting('http_timeout') = 10
             AND current_setting('http_retries') = 0
             AND NOT current_setting('force_download')
             AND current_setting('force_download_threshold') = 0
             AND NOT current_setting('auto_fallback_to_full_download')
             AND NOT current_setting('unsafe_disable_etag_checks')
             AND NOT current_setting('enable_http_logging')
             AND current_setting('enable_server_cert_verification')
             AND current_setting('enable_curl_server_cert_verification')",
                    [],
                    |r| r.get::<_, bool>(0)
                )
                .unwrap()
            );
            Ok(())
        })
        .await
        .unwrap();
    let opening = http
        .stats
        .transferred
        .load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        opening < http.stats.bytes.len() as u64,
        "opening transferred {opening} of {} bytes",
        http.stats.bytes.len()
    );
    let data = source.tile(TILE, None).await.unwrap();
    let tile = mvt::decode(&data).unwrap();
    assert_eq!(tile.layers[0].features.len(), 1);
    assert_eq!(tile.layers[0].features[0].id, Some(1));
    assert!(
        http.stats
            .ranges
            .lock()
            .unwrap()
            .iter()
            .all(|r| r.end - r.start < http.stats.bytes.len() as u64)
    );
    let transfer = http
        .stats
        .transferred
        .load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        transfer < http.stats.bytes.len() as u64,
        "open and tile transferred {transfer} bytes, total {}",
        http.stats.bytes.len()
    );
    eprintln!(
        "HTTP Range bytes: open={opening}, open+tile={transfer}, object={}",
        http.stats.bytes.len()
    );
    source.close().await.unwrap();
    source.close().await.unwrap();
    assert!(source.pool_status().connections == 0);
    assert!(source.tile(TILE, None).await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_authentication_is_session_scoped_and_errors_are_redacted() {
    use std::sync::atomic::Ordering;
    let fixture = fixture_points(&[(10.0, 60.0)], true);
    let http = TestHttp::start(std::fs::read(&fixture.path).unwrap()).await;
    http.stats.mode.store(6, Ordering::SeqCst);
    let signed_url = format!("{}?signature=private-signature", http.url);
    let error = GeoParquetSource::open_immutable_http(fixture.config(), &signed_url)
        .await
        .unwrap_err();
    assert!(!format!("{error:?}").contains("private-signature"));
    let headers = BTreeMap::from([("Authorization".into(), "Bearer test-'token".into())]);
    let source = GeoParquetSource::open_immutable_http_with_headers(
        fixture.config(),
        &signed_url,
        headers.clone(),
    )
    .await
    .unwrap();
    let clone = source.clone();
    assert!(!source.get_tile(TILE, None).await.unwrap().data.is_empty());
    let reopened =
        GeoParquetSource::open_immutable_http_with_headers(fixture.config(), &signed_url, headers)
            .await
            .unwrap();
    assert_ne!(source.content_version(), reopened.content_version());
    assert!(
        GeoParquetSource::open_immutable_http(fixture.config(), &signed_url)
            .await
            .is_err(),
        "secrets must not escape the source session"
    );
    source.close().await.unwrap();
    assert!(clone.get_tile(TILE, None).await.is_err());
    clone.close().await.unwrap();
    assert!(!reopened.get_tile(TILE, None).await.unwrap().data.is_empty());
    reopened.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s3_reads_directly_with_explicit_endpoint_credentials_and_session_token() {
    use crate::sources::GeoParquetS3Options;
    use std::sync::atomic::Ordering;
    let fixture = fixture_points(&[(10.0, 60.0)], true);
    let http = TestHttp::start(std::fs::read(&fixture.path).unwrap()).await;
    http.stats.mode.store(7, Ordering::SeqCst);
    let options = GeoParquetS3Options {
        endpoint: Some(
            http.url
                .strip_prefix("http://")
                .unwrap()
                .strip_suffix("/data.parquet")
                .unwrap()
                .into(),
        ),
        access_key_id: "test-key".into(),
        secret_access_key: "test-secret".into(),
        session_token: Some("test-session".into()),
        use_ssl: false,
        path_style: true,
        ..Default::default()
    };
    let source =
        GeoParquetSource::open_immutable_s3(fixture.config(), "s3://bucket/data.parquet", options)
            .await
            .unwrap();
    let actual = source.get_tile(TILE, None).await.unwrap();
    let local = GeoParquetSource::open(fixture.config()).unwrap();
    assert_eq!(actual.data, local.get_tile(TILE, None).await.unwrap().data);
    assert!(!http.stats.ranges.lock().unwrap().is_empty());
    source.close().await.unwrap();
    local.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_refuses_full_download_fallback_and_changed_objects() {
    use std::sync::atomic::Ordering;
    let fixture = fixture_points(&vec![(10.0, 60.0); 100_000], true);
    let http = TestHttp::start(std::fs::read(&fixture.path).unwrap()).await;
    http.stats.mode.store(2, Ordering::SeqCst);
    assert!(
        GeoParquetSource::open_immutable_http(fixture.config(), &http.url)
            .await
            .is_err(),
        "200 must not become a whole-file fallback"
    );
    http.stats.mode.store(0, Ordering::SeqCst);
    let source =
        GeoParquetSource::open_http_at_etag(fixture.config(), &http.url, "\"v1\"", BTreeMap::new())
            .await
            .unwrap();
    http.stats.mode.store(3, Ordering::SeqCst);
    let requests = http.stats.requests.load(Ordering::SeqCst);
    let source = source.with_file_lease("application-owned lease");
    assert!(!source.get_tile(TILE, None).await.unwrap().data.is_empty());
    assert_eq!(http.stats.requests.load(Ordering::SeqCst), requests);
    source.close().await.unwrap();
    for mode in [3, 8, 9, 10] {
        http.stats.mode.store(mode, Ordering::SeqCst);
        assert!(
            matches!(
                GeoParquetSource::open_http_at_etag(
                    fixture.config(),
                    &http.url,
                    "\"v1\"",
                    BTreeMap::new()
                )
                .await,
                Err(MaptileError::VersionChanged(_))
            ),
            "HTTP fixture mode {mode}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replacement_during_ranges_fails_the_opened_revision() {
    use std::sync::atomic::Ordering;
    let fixture = fixture_points(&vec![(10.0, 60.0); 100_000], true);
    let http = TestHttp::start(std::fs::read(&fixture.path).unwrap()).await;
    assert!(http.stats.bytes.len() > 1024 * 1024);
    http.stats.mode.store(5, Ordering::SeqCst);
    let read = tokio::spawn({
        let url = http.url.clone();
        let cfg = fixture.config();
        async move { GeoParquetSource::open_http_at_etag(cfg, &url, "\"v1\"", BTreeMap::new()).await }
    });
    tokio::time::timeout(Duration::from_secs(2), http.stats.blocked.notified())
        .await
        .unwrap();
    http.stats.mode.store(3, Ordering::SeqCst);
    http.stats.resume.close();
    assert!(matches!(
        read.await.unwrap(),
        Err(MaptileError::VersionChanged(_))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conditional_snapshot_caps_disk_and_unlinks_after_close() {
    use super::super::remote::capture_http_at_etag;
    let fixture = fixture_points(&[(10., 60.)], true);
    let http = TestHttp::start(std::fs::read(&fixture.path).unwrap()).await;
    let headers = BTreeMap::new();
    assert!(matches!(
        capture_http_at_etag(&http.url, "\"v1\"", &headers, 256).await,
        Err(MaptileError::PayloadTooLarge { limit: 256 })
    ));
    let file = capture_http_at_etag(&http.url, "\"v1\"", &headers, http.stats.bytes.len())
        .await
        .unwrap();
    let path = file.to_path_buf();
    assert_eq!(std::fs::read(&path).unwrap(), &http.stats.bytes[..]);
    let source =
        GeometrySource::open_snapshot(&fixture.config(), file, &QueryControl::pending()).unwrap();
    source.with_file_lease(Arc::new("caller lease"));
    assert!(path.is_file());
    source.close().await.unwrap();
    assert!(!path.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conditional_http_validates_ranges_credentials_redirects_and_deadline() {
    use std::sync::atomic::Ordering;
    let fixture = fixture_points(&[(10., 60.)], true);
    let http = TestHttp::start(std::fs::read(&fixture.path).unwrap()).await;
    let signed = format!("{}?signature=private-signature", http.url);
    let headers = BTreeMap::from([("Authorization".into(), "Bearer test-'token".into())]);
    http.stats.mode.store(6, Ordering::SeqCst);
    let error =
        GeoParquetSource::open_http_at_etag(fixture.config(), &signed, "\"v1\"", BTreeMap::new())
            .await
            .unwrap_err();
    assert!(!format!("{error:?}").contains("private-signature"));
    let source =
        GeoParquetSource::open_http_at_etag(fixture.config(), &signed, "\"v1\"", headers.clone())
            .await
            .unwrap();
    assert!(!source.get_tile(TILE, None).await.unwrap().data.is_empty());
    source.close().await.unwrap();
    for mode in [11, 12, 13] {
        http.stats.mode.store(mode, Ordering::SeqCst);
        let before = http.stats.requests.load(Ordering::SeqCst);
        let error = GeoParquetSource::open_http_at_etag(
            fixture.config(),
            &signed,
            "\"v1\"",
            headers.clone(),
        )
        .await
        .unwrap_err();
        if mode == 11 {
            assert!(matches!(error, MaptileError::BackendMsg(_)));
        } else {
            assert!(matches!(error, MaptileError::InvalidMetadata(_)));
        }
        assert_eq!(http.stats.requests.load(Ordering::SeqCst), before + 1);
    }
    http.stats.mode.store(5, Ordering::SeqCst);
    let mut config = fixture.config();
    config.query_options.timeout_ms = 300;
    assert!(matches!(
        GeoParquetSource::open_http_at_etag(config, &signed, "\"v1\"", headers).await,
        Err(MaptileError::Deadline)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_get_only_urls_open_without_head_support() {
    let fixture = fixture_points(&[(10.0, 60.0)], true);
    let http = TestHttp::start(std::fs::read(&fixture.path).unwrap()).await;
    http.stats
        .mode
        .store(4, std::sync::atomic::Ordering::SeqCst);
    let source = GeoParquetSource::open_immutable_http(
        fixture.config(),
        &format!(
            "{}?signature=get-only",
            http.url.replacen("http:", "HTTP:", 1)
        ),
    )
    .await
    .unwrap();
    assert!(!source.get_tile(TILE, None).await.unwrap().data.is_empty());
    source.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_interrupts_active_queries_and_rejects_waiters() {
    let fixture = fixture_points(&[(10.0, 60.0)], true);
    let mut source = GeometrySource::open(&fixture.config()).unwrap();
    source.query.sql = "SELECT NULL::BLOB, sum(i)::BIGINT FROM range(1000000000) t(i) WHERE i % ($z::INTEGER + 2) = $x::INTEGER AND i > $y::INTEGER".into();
    let source = Arc::new(source);
    let query_source = source.clone();
    let query = tokio::spawn(async move { query_source.tile(TILE, None).await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while source.pool_status().active_requests == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(2), source.close())
        .await
        .unwrap()
        .unwrap();
    assert!(query.await.unwrap().is_err());
    assert!(source.pool_status().connections == 0);
    assert_eq!(source.pool_status().active_requests, 0);
    assert!(source.tile(TILE, None).await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_open_error_detaches_before_outer_completion() {
    let http = TestHttp::start(b"not a parquet file".to_vec()).await;
    let running = QueryControl::pending();
    let _completion = FinishQuery(running.clone());
    let remote = RemoteLocation::http(&http.url, BTreeMap::new()).unwrap();
    let cfg = GeoParquetSourceConfig {
        geometry_column: Some("geom".into()),
        ..Default::default()
    };
    let result = tokio::task::spawn_blocking({
        let running = running.clone();
        move || GeometrySource::open_remote(&cfg, Arc::new(remote), &running, None)
    })
    .await
    .unwrap();
    assert!(result.is_err());
    running.assert_detached();
    running.cancel();
    assert!(running.cancelled());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_open_cancellation_interrupts_active_io_and_preserves_reason() {
    use std::sync::atomic::Ordering;
    let fixture = fixture_points(&[(10.0, 60.0)], true);
    let http = TestHttp::start(std::fs::read(&fixture.path).unwrap()).await;
    http.stats.mode.store(5, Ordering::SeqCst);
    let running = QueryControl::pending();
    let cancellation = running.watch(tokio::time::Instant::now() + Duration::from_secs(10));
    let config = fixture.config();
    let remote = RemoteLocation::http(&http.url, BTreeMap::new()).unwrap();
    let worker = tokio::task::spawn_blocking({
        let running = running.clone();
        move || {
            let _completion = FinishQuery(running.clone());
            let result = GeometrySource::open_remote(&config, Arc::new(remote), &running, None);
            running.assert_detached();
            result
        }
    });
    tokio::time::timeout(Duration::from_secs(2), http.stats.blocked.notified())
        .await
        .unwrap();
    drop(cancellation);
    assert!(running.cancelled());
    http.stats.resume.close();
    let result = tokio::time::timeout(Duration::from_secs(2), worker)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(result, Err(MaptileError::Cancelled)));
}

#[test]
fn queued_remote_open_cancellation_never_starts_network_io() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap()
        .block_on(async {
            use std::sync::atomic::Ordering;
            let fixture = fixture_points(&[(10.0, 60.0)], true);
            let http = TestHttp::start(std::fs::read(&fixture.path).unwrap()).await;
            let (entered, ready) = tokio::sync::oneshot::channel();
            let (release, wait) = std::sync::mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                let _ = entered.send(());
                let _ = wait.recv();
            });
            ready.await.unwrap();
            let mut opening = Box::pin(GeoParquetSource::open_immutable_http(
                fixture.config(),
                &http.url,
            ));
            assert!(futures_util::poll!(&mut opening).is_pending());
            drop(opening);
            release.send(()).unwrap();
            blocker.await.unwrap();
            tokio::time::timeout(Duration::from_secs(2), tokio::task::spawn_blocking(|| ()))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(http.stats.requests.load(Ordering::SeqCst), 0);
        });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_close_waits_for_duckdb_read_and_can_be_cancelled_and_retried() {
    use std::sync::atomic::Ordering;
    let fixture = fixture_points(&vec![(10.0, 60.0); 100_000], true);
    let http = TestHttp::start(std::fs::read(&fixture.path).unwrap()).await;
    let source = Arc::new(
        GeoParquetSource::open_immutable_http(fixture.config(), &http.url)
            .await
            .unwrap(),
    );
    http.stats.mode.store(5, Ordering::SeqCst);
    let reading = source.clone();
    let request = tokio::spawn(async move { reading.get_tile(TILE, None).await });
    tokio::time::timeout(Duration::from_secs(2), http.stats.blocked.notified())
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(10), source.close())
            .await
            .is_err()
    );
    http.stats.resume.close();
    tokio::time::timeout(Duration::from_secs(2), source.close())
        .await
        .unwrap()
        .unwrap();
    assert!(request.await.unwrap().is_err());
    assert!(source.get_tile(TILE, None).await.is_err());
    source.close().await.unwrap();
}

struct HttpStats {
    bytes: bytes::Bytes,
    ranges: Mutex<Vec<std::ops::Range<u64>>>,
    transferred: std::sync::atomic::AtomicU64,
    requests: std::sync::atomic::AtomicUsize,
    mode: std::sync::atomic::AtomicU8,
    blocked: Notify,
    resume: Semaphore,
}
struct TestHttp {
    url: String,
    stats: Arc<HttpStats>,
    task: tokio::task::JoinHandle<()>,
}
impl TestHttp {
    async fn start(bytes: Vec<u8>) -> Self {
        let stats = Arc::new(HttpStats {
            bytes: bytes.into(),
            ranges: Mutex::new(Vec::new()),
            transferred: 0.into(),
            requests: 0.into(),
            mode: 0.into(),
            blocked: Notify::new(),
            resume: Semaphore::new(0),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/data.parquet", listener.local_addr().unwrap());
        let app = axum::Router::new()
            .route("/data.parquet", axum::routing::any(test_http_request))
            .route(
                "/bucket/data.parquet",
                axum::routing::any(test_http_request),
            )
            .with_state(stats.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { url, stats, task }
    }
}
impl Drop for TestHttp {
    fn drop(&mut self) {
        self.stats.resume.close();
        self.task.abort();
    }
}
async fn test_http_request(
    axum::extract::State(stats): axum::extract::State<Arc<HttpStats>>,
    method: axum::http::Method,
    headers: axum::http::HeaderMap,
) -> axum::http::Response<axum::body::Body> {
    use axum::body::Body;
    use axum::http::{Response, StatusCode, header};
    use std::sync::atomic::Ordering;
    stats.requests.fetch_add(1, Ordering::SeqCst);
    let mode = stats.mode.load(Ordering::SeqCst);
    if (mode == 6
        && headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            != Some("Bearer test-'token"))
        || (mode == 7
            && (!headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| {
                    v.starts_with("AWS4-HMAC-SHA256 ") && v.contains("Credential=test-key/")
                })
                || headers
                    .get("x-amz-security-token")
                    .and_then(|v| v.to_str().ok())
                    != Some("test-session")))
    {
        return Response::builder()
            .status(StatusCode::FORBIDDEN)
            .body(Body::empty())
            .unwrap();
    }
    if mode == 4 && method == axum::http::Method::HEAD {
        return Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .body(Body::empty())
            .unwrap();
    }
    if mode == 9 {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::empty())
            .unwrap();
    }
    if mode == 11 {
        return Response::builder()
            .status(StatusCode::TEMPORARY_REDIRECT)
            .header(header::LOCATION, "/bucket/data.parquet")
            .body(Body::empty())
            .unwrap();
    }
    let etag = if matches!(mode, 3 | 8) {
        "\"v2\""
    } else {
        "\"v1\""
    };
    if mode != 8
        && headers
            .get(header::IF_MATCH)
            .and_then(|tag| tag.to_str().ok())
            .is_some_and(|tag| tag != etag)
    {
        return Response::builder()
            .status(StatusCode::PRECONDITION_FAILED)
            .body(Body::empty())
            .unwrap();
    }
    let mut response = Response::builder();
    if mode != 10 {
        response = response.header(header::ETAG, etag);
    }
    if method == axum::http::Method::HEAD {
        return response
            .header(header::CONTENT_LENGTH, stats.bytes.len())
            .body(Body::empty())
            .unwrap();
    }
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| test_range(v, stats.bytes.len() as u64))
        .expect("every data GET must carry a valid Range");
    stats.ranges.lock().unwrap().push(range.clone());
    if mode == 5 {
        stats.blocked.notify_one();
        let _ = stats.resume.acquire().await;
    }
    if mode == 2 {
        return response.body(Body::from(stats.bytes.clone())).unwrap();
    }
    stats
        .transferred
        .fetch_add(range.end - range.start, Ordering::SeqCst);
    let body_end = if mode == 13 { range.end - 1 } else { range.end };
    response
        .status(StatusCode::PARTIAL_CONTENT)
        .header(
            header::CONTENT_RANGE,
            format!(
                "bytes {}-{}/{}",
                if mode == 12 {
                    range.start + 1
                } else {
                    range.start
                },
                range.end - 1,
                stats.bytes.len()
            ),
        )
        .header(header::CONTENT_LENGTH, body_end - range.start)
        .body(Body::from(
            stats.bytes.slice(range.start as usize..body_end as usize),
        ))
        .unwrap()
}

fn test_range(value: &str, size: u64) -> Option<std::ops::Range<u64>> {
    let (start, end) = value.strip_prefix("bytes=")?.split_once('-')?;
    if start.is_empty() {
        return Some(size.saturating_sub(end.parse().ok()?)..size);
    }
    let start = start.parse::<u64>().ok()?;
    let end = if end.is_empty() {
        size
    } else {
        end.parse::<u64>().ok()?.checked_add(1)?.min(size)
    };
    (start < end).then_some(start..end)
}

struct Fixture {
    _dir: tempfile::TempDir,
    path: PathBuf,
}
impl Fixture {
    fn config(&self) -> GeoParquetSourceConfig {
        GeoParquetSourceConfig {
            path: self.path.clone(),
            geometry_column: Some("geometry".into()),
            trust_covering: true,
            id_column: Some("id".into()),
            layer_name: "places".into(),
            ..Default::default()
        }
    }
}
fn fixture_points(points: &[(f64, f64)], covering: bool) -> Fixture {
    fixture(
        points.iter().map(|(x, y)| point(*x, *y)).collect(),
        None,
        covering,
    )
}
fn fixture(rows: Vec<Option<Vec<u8>>>, crs: Option<Value>, covering: bool) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("geoparquet's file.parquet");
    let bboxes: Vec<_> = rows
        .iter()
        .map(|w| w.as_ref().and_then(|w| crate::wkb_mvt::wkb_bbox(w)))
        .collect();
    let bbox_fields: Vec<_> = ["xmin", "ymin", "xmax", "ymax"]
        .into_iter()
        .map(|n| Field::new(n, DataType::Float64, true))
        .collect();
    let bbox = StructArray::from(
        bbox_fields
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let values: Vec<Option<f64>> = bboxes
                    .iter()
                    .map(|b| b.map(|b| [b.0, b.1, b.2, b.3][i]))
                    .collect();
                (
                    Arc::new(f.clone()),
                    Arc::new(Float64Array::from(values)) as ArrayRef,
                )
            })
            .collect::<Vec<_>>(),
    );
    let schema = Arc::new(Schema::new(vec![
        Field::new("geometry", DataType::Binary, true),
        Field::new("id", DataType::Int64, false),
        Field::new("name", DataType::Utf8, false),
        Field::new("unsigned", DataType::UInt64, false),
        Field::new("bbox", DataType::Struct(bbox_fields.into()), false),
    ]));
    let count = rows.len();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(BinaryArray::from_iter(rows.iter().map(|r| r.as_deref()))),
            Arc::new(Int64Array::from_iter_values(1..=count as i64)),
            Arc::new(StringArray::from_iter_values(
                (0..count).map(|i| format!("row-{i}")),
            )),
            Arc::new(UInt64Array::from(vec![u64::MAX; count])),
            Arc::new(bbox),
        ],
    )
    .unwrap();
    let mut column =
        serde_json::json!({"encoding":"WKB","geometry_types":["Point","LineString","Polygon"]});
    if let Some(crs) = crs {
        column["crs"] = crs;
    }
    if covering {
        column["covering"] = serde_json::json!({"bbox":{"xmin":["bbox","xmin"],"ymin":["bbox","ymin"],"xmax":["bbox","xmax"],"ymax":["bbox","ymax"]}});
    }
    let geo=serde_json::json!({"version":"1.1.0","primary_column":"geometry","columns":{"geometry":column}}).to_string();
    let props = WriterProperties::builder()
        .set_max_row_group_row_count(Some(2048))
        .set_key_value_metadata(Some(vec![KeyValue::new("geo".into(), geo)]))
        .build();
    let mut writer =
        ArrowWriter::try_new(std::fs::File::create(&path).unwrap(), schema, Some(props)).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
    Fixture { _dir: dir, path }
}
fn point(x: f64, y: f64) -> Option<Vec<u8>> {
    let mut w = vec![1];
    w.extend_from_slice(&1u32.to_le_bytes());
    w.extend_from_slice(&x.to_le_bytes());
    w.extend_from_slice(&y.to_le_bytes());
    Some(w)
}
fn line(points: &[(f64, f64)]) -> Option<Vec<u8>> {
    let mut w = vec![1];
    w.extend_from_slice(&2u32.to_le_bytes());
    w.extend_from_slice(&(points.len() as u32).to_le_bytes());
    for (x, y) in points {
        w.extend_from_slice(&x.to_le_bytes());
        w.extend_from_slice(&y.to_le_bytes());
    }
    Some(w)
}
fn polygon() -> Option<Vec<u8>> {
    let mut w = vec![1];
    w.extend_from_slice(&3u32.to_le_bytes());
    w.extend_from_slice(&2u32.to_le_bytes());
    for ring in [
        [
            (-10f64, 50f64),
            (40., 50.),
            (40., 70.),
            (-10., 70.),
            (-10., 50.),
        ],
        [(10., 55.), (10., 60.), (20., 60.), (20., 55.), (10., 55.)],
    ] {
        w.extend_from_slice(&5u32.to_le_bytes());
        for (x, y) in ring {
            w.extend_from_slice(&x.to_le_bytes());
            w.extend_from_slice(&y.to_le_bytes());
        }
    }
    Some(w)
}
fn unzig(v: u32) -> i32 {
    ((v >> 1) as i32) ^ (-((v & 1) as i32))
}
fn coordinates(commands: &[u32]) -> Vec<(i32, i32)> {
    let (mut i, mut x, mut y) = (0, 0, 0);
    let mut out = Vec::new();
    while i < commands.len() {
        let cmd = commands[i] & 7;
        let count = commands[i] >> 3;
        i += 1;
        if cmd == 7 {
            continue;
        }
        assert!(cmd == 1 || cmd == 2);
        for _ in 0..count {
            x += unzig(commands[i]);
            y += unzig(commands[i + 1]);
            i += 2;
            out.push((x, y));
        }
    }
    out
}
fn properties<'a>(
    layer: &'a mvt::proto::Layer,
    feature: &mvt::proto::Feature,
) -> BTreeMap<&'a str, &'a mvt::proto::Value> {
    feature
        .tags
        .chunks(2)
        .map(|t| {
            (
                layer.keys[t[0] as usize].as_str(),
                &layer.values[t[1] as usize],
            )
        })
        .collect()
}
