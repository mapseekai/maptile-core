//! Tests for `PostgresTableSource` — require `DATABASE_URL`.

#[path = "../../common/mod.rs"]
mod common;

use maptile_core::sources::postgres::{
    PoolSettings, PostgresPool, PostgresTableConfig, PostgresTableSource,
};
use maptile_core::sources::{TileCoord, TileSource};
use std::sync::Arc;

async fn make_pool(dsn: &str) -> Arc<PostgresPool> {
    Arc::new(
        PostgresPool::connect(dsn, PoolSettings::default())
            .await
            .expect("connect"),
    )
}

async fn create_simple_table(dsn: &str, schema: &str, table: &str) {
    let (client, conn) = tokio_postgres::connect(dsn, tokio_postgres::NoTls)
        .await
        .expect("connect for setup");
    tokio::spawn(async move {
        let _ = conn.await;
    });
    client
        .batch_execute(&format!(
            r#"
            CREATE TABLE "{schema}"."{table}" (
                id bigserial PRIMARY KEY,
                name text,
                geom geometry(Point, 4326)
            );
            INSERT INTO "{schema}"."{table}" (name, geom)
            VALUES
                ('a', ST_SetSRID(ST_MakePoint(0, 0), 4326)),
                ('b', ST_SetSRID(ST_MakePoint(1, 1), 4326));
            CREATE INDEX ON "{schema}"."{table}" USING gist (geom);
            "#
        ))
        .await
        .expect("create table");
}

fn cfg_for(schema: &str, table: &str) -> PostgresTableConfig {
    PostgresTableConfig::new("t", schema, table)
}

#[tokio::test]
async fn batch_matches_individual_tiles_including_empty_and_duplicate_coordinates() {
    let Some(dsn) = common::postgres::pg_dsn() else {
        return;
    };
    for srid in [4326, 3857] {
        let guard = common::postgres::SchemaGuard::create(&dsn).await;
        create_simple_table(&dsn, &guard.name, "batch").await;
        let pool = make_pool(&dsn).await;
        if srid == 3857 {
            pool.get().await.unwrap().batch_execute(&format!(
            "ALTER TABLE \"{}\".batch ALTER COLUMN geom TYPE geometry(Point,3857) USING ST_Transform(geom,3857)", guard.name
        )).await.unwrap();
        }
        let source = PostgresTableSource::open(pool, cfg_for(&guard.name, "batch"))
            .await
            .unwrap();
        let coords = [
            TileCoord {
                z: 14,
                x: 8192,
                y: 8192,
            },
            TileCoord {
                z: 14,
                x: 8200,
                y: 8180,
            },
            TileCoord {
                z: 14,
                x: 8237,
                y: 8146,
            },
            TileCoord {
                z: 14,
                x: 8192,
                y: 8192,
            },
            TileCoord { z: 0, x: 0, y: 0 },
            TileCoord { z: 14, x: 0, y: 0 },
        ];
        let batch = source.read_tile_batch(&coords, None).await.unwrap();
        assert_eq!(batch.len(), coords.len());
        assert!(batch[1].data.is_empty());
        assert!(!batch[0].data.is_empty());
        let outside = source.read_tile_batch(&coords[4..], None).await.unwrap();
        assert_eq!(outside.len(), 2);
        assert!(outside.iter().all(|tile| tile.data.is_empty()));
        for (coord, actual) in coords.into_iter().zip(batch) {
            let single = source.get_tile(coord, None).await.unwrap();
            let expected =
                maptile_core::transport::decode(&single.data, single.info.encoding).unwrap();
            assert_eq!(actual.data, expected);
            assert_eq!(
                actual.info.encoding,
                maptile_core::sources::TileEncoding::Uncompressed
            );
        }
    }
}

#[tokio::test]
async fn batch_coordinates_are_not_shadowed_by_source_identifiers() {
    use maptile_core::{
        filter::parse_cql2,
        sources::{TileQuery, mvt},
    };
    let Some(dsn) = common::postgres::pg_dsn() else {
        return;
    };
    for (table, coordinate_columns) in [
        ("batch_coords", false),
        ("batch_coords", true),
        ("batch_tiles", false),
        ("source_features", false),
        ("tile", false),
        ("Mixed source", false),
    ] {
        let guard = common::postgres::SchemaGuard::create(&dsn).await;
        create_simple_table(&dsn, &guard.name, table).await;
        let pool = make_pool(&dsn).await;
        if coordinate_columns {
            pool.get()
                .await
                .unwrap()
                .batch_execute(&format!(
                    "ALTER TABLE \"{}\".\"{table}\" ADD COLUMN z int DEFAULT 0, \
                 ADD COLUMN x int DEFAULT 0, ADD COLUMN y int DEFAULT 0, \
                 ADD COLUMN enabled boolean DEFAULT false",
                    guard.name
                ))
                .await
                .unwrap();
        }
        if table == "Mixed source" {
            pool.get()
                .await
                .unwrap()
                .batch_execute(&format!(
                    "ALTER TABLE \"{}\".\"{table}\" RENAME COLUMN geom TO batch_coords",
                    guard.name
                ))
                .await
                .unwrap();
        }
        let mut config = cfg_for(&guard.name, table);
        config.properties = Some(vec!["name".into()]);
        config.id_column = Some("id".into());
        let source = PostgresTableSource::open(pool, config)
            .await
            .unwrap()
            .with_allowed_columns(Arc::new(std::collections::HashSet::from(["name".into()])));
        let coords = [
            TileCoord {
                z: 8,
                x: 128,
                y: 127,
            },
            TileCoord {
                z: 8,
                x: 255,
                y: 255,
            },
            TileCoord {
                z: 8,
                x: 128,
                y: 127,
            },
        ];
        let filter = parse_cql2("name = 'a'").unwrap();
        let query = TileQuery {
            filter: Some(&filter),
            ..TileQuery::default()
        };
        for query in [None, Some(&query)] {
            let mut expected = Vec::new();
            for &coord in &coords {
                let tile = source.get_tile(coord, query).await.unwrap();
                expected
                    .push(maptile_core::transport::decode(&tile.data, tile.info.encoding).unwrap());
            }
            assert!(
                !expected[0].is_empty(),
                "single tile must contain data for {table}"
            );
            let actual = source.read_tile_batch(&coords, query).await.unwrap();
            assert_eq!(actual.len(), coords.len());
            for (tile, expected) in actual.iter().zip(expected) {
                assert_eq!(tile.data.as_ref(), expected, "source table {table}");
            }
            let decoded = mvt::decode(&actual[0].data).unwrap();
            assert_eq!(decoded.layers[0].name, table);
            assert_eq!(decoded.layers[0].keys, ["name"]);
            assert!(
                decoded.layers[0]
                    .features
                    .iter()
                    .all(|feature| feature.id.is_some())
            );
        }
    }
}

#[tokio::test]
async fn batch_byte_budget_resumes_on_the_same_pool_connection() {
    let Some(dsn) = common::postgres::pg_dsn() else {
        return;
    };
    let guard = common::postgres::SchemaGuard::create(&dsn).await;
    create_simple_table(&dsn, &guard.name, "large_batch").await;
    let pool = Arc::new(
        PostgresPool::connect(
            &dsn,
            PoolSettings {
                max_size: 1,
                ..PoolSettings::default()
            },
        )
        .await
        .unwrap(),
    );
    pool.get().await.unwrap().batch_execute(&format!(
        "ALTER TABLE \"{s}\".large_batch ADD COLUMN payload text; \
         UPDATE \"{s}\".large_batch SET payload=repeat('x', 5*1024*1024); \
         CREATE SEQUENCE \"{s}\".batch_reads; \
         CREATE VIEW \"{s}\".counted_batch AS \
         SELECT id, name, geom, left(payload || nextval('{s}.batch_reads')::text, length(payload)) AS payload \
         FROM \"{s}\".large_batch", s=guard.name
    )).await.unwrap();
    let source = PostgresTableSource::open(pool.clone(), cfg_for(&guard.name, "counted_batch"))
        .await
        .unwrap();
    pool.get()
        .await
        .unwrap()
        .query_one(
            &format!("SELECT setval('{}.batch_reads', 1, false)", guard.name),
            &[],
        )
        .await
        .unwrap();
    let coords = [TileCoord {
        z: 14,
        x: 8192,
        y: 8192,
    }; 3];
    let first = source.read_tile_batch(&coords, None).await.unwrap();
    assert_eq!(first.len(), 2);
    let rendered: i64 = pool
        .get()
        .await
        .unwrap()
        .query_one(
            &format!("SELECT last_value FROM \"{}\".batch_reads", guard.name),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        rendered, 2,
        "the byte budget must also stop SQL from rendering the suffix"
    );
    let rest = source
        .read_tile_batch(&coords[first.len()..], None)
        .await
        .unwrap();
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0].data, first[0].data);
    let rendered: i64 = pool
        .get()
        .await
        .unwrap()
        .query_one(
            &format!("SELECT last_value FROM \"{}\".batch_reads", guard.name),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(rendered, 3, "resuming must compute each coordinate once");
}

#[tokio::test]
async fn batch_preserves_projected_properties_feature_ids_and_filter_validation() {
    use maptile_core::{
        filter::parse_cql2,
        sources::{TileQuery, mvt},
    };
    let Some(dsn) = common::postgres::pg_dsn() else {
        return;
    };
    let guard = common::postgres::SchemaGuard::create(&dsn).await;
    create_simple_table(&dsn, &guard.name, "filtered_batch").await;
    let mut config = cfg_for(&guard.name, "filtered_batch");
    config.properties = Some(vec!["name".into()]);
    config.id_column = Some("id".into());
    let source = PostgresTableSource::open(make_pool(&dsn).await, config)
        .await
        .unwrap()
        .with_allowed_columns(Arc::new(std::collections::HashSet::from(["name".into()])));
    let coords = [
        TileCoord {
            z: 8,
            x: 128,
            y: 127,
        },
        TileCoord { z: 0, x: 0, y: 0 },
    ];
    let unfiltered = source.read_tile_batch(&coords, None).await.unwrap();
    assert!(unfiltered[1].data.is_empty());
    let filter = parse_cql2("name = 'a'").unwrap();
    let query = TileQuery {
        filter: Some(&filter),
        ..TileQuery::default()
    };
    let filtered = source.read_tile_batch(&coords, Some(&query)).await.unwrap();
    for (coord, tile) in coords.into_iter().zip(filtered) {
        let single = source.get_tile(coord, Some(&query)).await.unwrap();
        assert_eq!(
            tile.data,
            maptile_core::transport::decode(&single.data, single.info.encoding).unwrap()
        );
        assert_ne!(tile.data, unfiltered[0].data);
        let decoded = mvt::decode(&tile.data).unwrap();
        assert_eq!(decoded.layers[0].keys, ["name"]);
        assert_eq!(decoded.layers[0].features.len(), 1);
        assert_eq!(decoded.layers[0].features[0].id, Some(1));
    }
    let rejected = parse_cql2("secret = 'a'").unwrap();
    let query = TileQuery {
        filter: Some(&rejected),
        ..TileQuery::default()
    };
    assert!(source.read_tile_batch(&coords, Some(&query)).await.is_err());
}

#[tokio::test]
async fn cancelled_batch_stops_sql_and_releases_its_pool_slot() {
    use std::time::Duration;
    let Some(dsn) = common::postgres::pg_dsn() else {
        return;
    };
    let guard = common::postgres::SchemaGuard::create(&dsn).await;
    create_simple_table(&dsn, &guard.name, "batch_cancel").await;
    let (monitor, connection) = tokio_postgres::connect(&dsn, tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    monitor.batch_execute(&format!(
        "CREATE FUNCTION \"{s}\".slow_name(value text) RETURNS text LANGUAGE plpgsql VOLATILE AS \
         $$ BEGIN PERFORM pg_sleep(10); RETURN value; END $$; \
         CREATE VIEW \"{s}\".slow_batch AS SELECT id, \"{s}\".slow_name(name) AS name, geom FROM \"{s}\".batch_cancel", s=guard.name
    )).await.unwrap();
    let pool = Arc::new(
        PostgresPool::connect(
            &dsn,
            PoolSettings {
                max_size: 1,
                ..PoolSettings::default()
            },
        )
        .await
        .unwrap(),
    );
    let mut config = cfg_for(&guard.name, "slow_batch");
    config.bounds = Some([-180.0, -85.0, 180.0, 85.0]);
    let source = Arc::new(
        PostgresTableSource::open(pool.clone(), config)
            .await
            .unwrap(),
    );
    let connection = pool.get().await.unwrap();
    let pid: i32 = connection
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    let delayed_cancel = connection.cancel_token();
    drop(connection);
    let read = tokio::spawn(async move {
        source
            .read_tile_batch(
                &[TileCoord {
                    z: 14,
                    x: 8192,
                    y: 8192,
                }; 4],
                None,
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let row = monitor.query_one("SELECT COALESCE(bool_or(wait_event='PgSleep'),false) FROM pg_stat_activity WHERE pid=$1", &[&pid]).await.unwrap();
            if row.get::<_,bool>(0) { break; }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }).await.expect("batch SQL must reach the blocking function");
    read.abort();
    assert!(read.await.unwrap_err().is_cancelled());
    let resumed = tokio::time::timeout(Duration::from_millis(750), async {
        pool.get()
            .await
            .unwrap()
            .query_one("SELECT 1::int4", &[])
            .await
            .unwrap()
            .get::<_, i32>(0)
    })
    .await;
    let stopped = tokio::time::timeout(Duration::from_millis(750), async {
        loop {
            let row = monitor
                .query_one(
                    "SELECT count(*) FROM pg_stat_activity WHERE pid=$1 AND state='active'",
                    &[&pid],
                )
                .await
                .unwrap();
            if row.get::<_, i64>(0) == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    monitor
        .query_one("SELECT pg_cancel_backend($1)", &[&pid])
        .await
        .unwrap();
    assert_eq!(
        resumed.unwrap(),
        1,
        "cancelled batch retained a busy connection"
    );
    assert!(
        stopped.is_ok(),
        "cancelled batch SQL kept running on PostgreSQL"
    );
    let next = pool.get().await.unwrap();
    let new_pid: i32 = next
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    assert_ne!(
        pid, new_pid,
        "an interrupted backend must not return to the pool"
    );
    let query = next.query_one("SELECT pg_sleep(0.2), 1::int4", &[]);
    let replay = async {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let row = monitor.query_one("SELECT COALESCE(bool_or(wait_event='PgSleep'),false) FROM pg_stat_activity WHERE pid=$1", &[&new_pid]).await.unwrap();
                if row.get::<_,bool>(0) { break; }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }).await.expect("replacement query must be running before replaying the old cancel");
        delayed_cancel
            .cancel_query(tokio_postgres::NoTls)
            .await
            .unwrap();
    };
    let (result, ()) = tokio::join!(query, replay);
    assert_eq!(
        result
            .expect("delayed cancellation affected a replacement query")
            .get::<_, i32>(1),
        1
    );
}

#[tokio::test]
async fn empty_table_keeps_extent_absent_and_serves_an_empty_tile() {
    let Some(dsn) = common::postgres::pg_dsn() else {
        return;
    };
    let guard = common::postgres::SchemaGuard::create(&dsn).await;
    create_simple_table(&dsn, &guard.name, "empty").await;
    let pool = make_pool(&dsn).await;
    pool.get()
        .await
        .unwrap()
        .batch_execute(&format!("TRUNCATE TABLE \"{}\".empty", guard.name))
        .await
        .unwrap();
    let source = PostgresTableSource::open(pool, cfg_for(&guard.name, "empty"))
        .await
        .unwrap();
    assert!(source.tilejson().bounds.is_none());
    assert!(source.tilejson().center.is_none());
    assert!(source.table_info().bounds.is_none());
    let tile = source
        .get_tile(TileCoord { z: 0, x: 0, y: 0 }, None)
        .await
        .unwrap();
    assert!(tile.data.is_empty());
}

#[tokio::test]
async fn point_table_keeps_its_exact_degenerate_extent() {
    let Some(dsn) = common::postgres::pg_dsn() else {
        return;
    };
    let guard = common::postgres::SchemaGuard::create(&dsn).await;
    create_simple_table(&dsn, &guard.name, "point").await;
    let pool = make_pool(&dsn).await;
    pool.get()
        .await
        .unwrap()
        .batch_execute(&format!(
            "DELETE FROM \"{}\".point WHERE name='b'",
            guard.name
        ))
        .await
        .unwrap();
    let source = PostgresTableSource::open(pool, cfg_for(&guard.name, "point"))
        .await
        .unwrap();
    let bounds = source.tilejson().bounds.unwrap();
    assert_eq!(
        [bounds.left, bounds.bottom, bounds.right, bounds.top],
        [0.0; 4]
    );
}

#[tokio::test]
async fn opening_source_leaves_busy_connections_to_tile_queries() {
    let Some(dsn) = common::postgres::pg_dsn() else {
        return;
    };
    let g = common::postgres::SchemaGuard::create(&dsn).await;
    create_simple_table(&dsn, &g.name, "t").await;
    let pool = Arc::new(
        PostgresPool::connect(
            &dsn,
            PoolSettings {
                max_size: 2,
                ..PoolSettings::default()
            },
        )
        .await
        .unwrap(),
    );
    let busy = pool.get().await.unwrap();
    let source = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        PostgresTableSource::open(pool.clone(), cfg_for(&g.name, "t")),
    )
    .await
    .expect("source registration waited for a busy query connection")
    .unwrap();
    let coord = TileCoord {
        z: 8,
        x: 128,
        y: 127,
    };
    let first = source.get_tile(coord, None).await.unwrap();
    assert!(!first.data.is_empty());
    assert_eq!(first.data, source.get_tile(coord, None).await.unwrap().data);
    assert_eq!(
        busy.query_one("SELECT 1::int4", &[])
            .await
            .unwrap()
            .get::<_, i32>(0),
        1
    );
}

#[tokio::test]
#[ignore = "manual PostgreSQL registration/tiles performance comparison"]
async fn registration_under_tile_load() {
    use std::time::{Duration, Instant};
    let dsn = common::postgres::pg_dsn().expect("DATABASE_URL required for performance comparison");
    let g = common::postgres::SchemaGuard::create(&dsn).await;
    create_simple_table(&dsn, &g.name, "t").await;
    let started = Instant::now();
    let pool = Arc::new(
        PostgresPool::connect(
            &dsn,
            PoolSettings {
                max_size: 16,
                pre_warm: true,
                ..PoolSettings::default()
            },
        )
        .await
        .unwrap(),
    );
    let connect_ms = started.elapsed().as_secs_f64() * 1000.0;
    let started = Instant::now();
    let source = Arc::new(
        PostgresTableSource::open(pool.clone(), cfg_for(&g.name, "t"))
            .await
            .unwrap(),
    );
    let initial_open_ms = started.elapsed().as_secs_f64() * 1000.0;
    let coord = TileCoord {
        z: 8,
        x: 128,
        y: 127,
    };
    let started = Instant::now();
    assert!(!source.get_tile(coord, None).await.unwrap().data.is_empty());
    let first_tile_ms = started.elapsed().as_secs_f64() * 1000.0;
    futures_util::future::try_join_all((0..16).map(|_| source.get_tile(coord, None)))
        .await
        .unwrap();
    let barrier = Arc::new(tokio::sync::Barrier::new(9));
    let mut workers = Vec::new();
    for _ in 0..8 {
        let source = source.clone();
        let barrier = barrier.clone();
        workers.push(tokio::spawn(async move {
            barrier.wait().await;
            let mut latencies = Vec::new();
            for _ in 0..200 {
                let started = Instant::now();
                assert!(!source.get_tile(coord, None).await.unwrap().data.is_empty());
                latencies.push(started.elapsed().as_secs_f64() * 1000.0);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            latencies
        }));
    }
    barrier.wait().await;
    let started = Instant::now();
    futures_util::future::try_join_all(
        (0..12).map(|_| PostgresTableSource::open(pool.clone(), cfg_for(&g.name, "t"))),
    )
    .await
    .unwrap();
    let registration_ms = started.elapsed().as_secs_f64() * 1000.0;
    let mut latencies = Vec::new();
    for worker in workers {
        latencies.extend(worker.await.unwrap());
    }
    latencies.sort_by(f64::total_cmp);
    println!(
        "PERF {}",
        serde_json::json!({
            "pool_size":16,"registrations":12,"workers":8,"tile_requests":latencies.len(),
            "connect_ms":connect_ms,"initial_open_ms":initial_open_ms,"first_tile_ms":first_tile_ms,
            "ready_first_tile_ms":connect_ms+initial_open_ms+first_tile_ms,"registration_ms":registration_ms,
            "tile_p50_ms":latencies[latencies.len()/2],
            "tile_p95_ms":latencies[latencies.len()*95/100],
            "tile_p99_ms":latencies[latencies.len()*99/100],
            "tile_max_ms":latencies.last()
        })
    );
}

#[tokio::test]
async fn edited_revision_bypasses_old_tiles_and_rediscovers_fields() {
    let Some(dsn) = common::postgres::pg_dsn() else {
        return;
    };
    let g = common::postgres::SchemaGuard::create(&dsn).await;
    create_simple_table(&dsn, &g.name, "t").await;
    let pool = make_pool(&dsn).await;
    let cache = Arc::new(maptile_core::cache::TileCache::new(1, None, None).unwrap());
    let old = PostgresTableSource::open(pool.clone(), cfg_for(&g.name, "t"))
        .await
        .unwrap()
        .with_edit_revision(Some("before".into()))
        .with_cache(cache.clone());
    let old_version = old.content_version();
    let coord = TileCoord {
        z: 8,
        x: 128,
        y: 127,
    };
    let before = old.get_tile(coord, None).await.unwrap();
    let conn = pool.get().await.unwrap();
    conn.batch_execute(&format!("ALTER TABLE \"{}\".t ADD COLUMN extra text; UPDATE \"{}\".t SET extra='new'; ALTER TABLE \"{}\".t DROP COLUMN name",g.name,g.name,g.name)).await.unwrap();
    drop(conn);
    let fresh = PostgresTableSource::open(pool, cfg_for(&g.name, "t"))
        .await
        .unwrap()
        .with_edit_revision(Some("after".into()))
        .with_cache(cache);
    assert_ne!(old_version, fresh.content_version());
    assert_eq!(
        fresh.tilejson().other.get("mapseek_revision"),
        Some(&serde_json::json!("after"))
    );
    assert_eq!(
        fresh.content_version(),
        fresh.clone_source().content_version()
    );
    assert!(fresh.table_info().properties.contains(&"extra".into()));
    assert!(!fresh.table_info().properties.contains(&"name".into()));
    let after = fresh.get_tile(coord, None).await.unwrap();
    assert_ne!(before.data, after.data);
}

#[tokio::test]
async fn projection_and_coverage_hints_partition_the_same_edit_revision() {
    let Some(dsn) = common::postgres::pg_dsn() else {
        return;
    };
    let guard = common::postgres::SchemaGuard::create(&dsn).await;
    create_simple_table(&dsn, &guard.name, "t").await;
    let pool = make_pool(&dsn).await;
    let mut cfg = cfg_for(&guard.name, "t");
    cfg.properties = Some(vec!["name".into()]);
    let all = PostgresTableSource::open(pool.clone(), cfg.clone())
        .await
        .unwrap()
        .with_edit_revision(Some("same-revision".into()));
    cfg.properties = Some(Vec::new());
    let projected = PostgresTableSource::open(pool.clone(), cfg.clone())
        .await
        .unwrap()
        .with_edit_revision(Some("same-revision".into()));
    assert_ne!(all.content_version(), projected.content_version());
    cfg.bounds = Some([-180.0, -85.0, 180.0, 85.0]);
    let coverage = PostgresTableSource::open(pool, cfg)
        .await
        .unwrap()
        .with_edit_revision(Some("same-revision".into()));
    assert_ne!(coverage.content_version(), projected.content_version());
}

#[tokio::test]
async fn auto_discovers_geometry_column_and_srid() {
    let dsn = match common::postgres::pg_dsn() {
        Some(d) => d,
        None => return,
    };
    let g = common::postgres::SchemaGuard::create(&dsn).await;
    create_simple_table(&dsn, &g.name, "t").await;
    let src = PostgresTableSource::open(make_pool(&dsn).await, cfg_for(&g.name, "t"))
        .await
        .unwrap();
    let info = src.table_info();
    assert_eq!(info.geometry_column, "geom");
    assert_eq!(info.srid, 4326);
    assert!(info.has_spatial_index);
}

#[tokio::test]
async fn auto_discovers_properties_excluding_geometry() {
    let dsn = match common::postgres::pg_dsn() {
        Some(d) => d,
        None => return,
    };
    let g = common::postgres::SchemaGuard::create(&dsn).await;
    create_simple_table(&dsn, &g.name, "t").await;
    let src = PostgresTableSource::open(make_pool(&dsn).await, cfg_for(&g.name, "t"))
        .await
        .unwrap();
    let info = src.table_info();
    assert!(info.properties.contains(&"id".to_string()));
    assert!(info.properties.contains(&"name".to_string()));
    assert!(!info.properties.contains(&"geom".to_string()));
}

#[tokio::test]
async fn open_fails_when_no_geometry_column() {
    let dsn = match common::postgres::pg_dsn() {
        Some(d) => d,
        None => return,
    };
    let g = common::postgres::SchemaGuard::create(&dsn).await;
    let (client, conn) = tokio_postgres::connect(&dsn, tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });
    client
        .batch_execute(&format!(r#"CREATE TABLE "{}"."plain" (id int)"#, g.name))
        .await
        .unwrap();
    let res = PostgresTableSource::open(make_pool(&dsn).await, cfg_for(&g.name, "plain")).await;
    assert!(matches!(
        res,
        Err(maptile_core::error::MaptileError::Config(_))
    ));
}

#[tokio::test]
async fn open_fails_when_multiple_geometry_columns_unspecified() {
    let dsn = match common::postgres::pg_dsn() {
        Some(d) => d,
        None => return,
    };
    let g = common::postgres::SchemaGuard::create(&dsn).await;
    let (client, conn) = tokio_postgres::connect(&dsn, tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });
    client
        .batch_execute(&format!(
            r#"CREATE TABLE "{}"."dual" (
                 id int,
                 g1 geometry(Point, 4326),
                 g2 geometry(Point, 4326)
               )"#,
            g.name
        ))
        .await
        .unwrap();
    let res = PostgresTableSource::open(make_pool(&dsn).await, cfg_for(&g.name, "dual")).await;
    assert!(matches!(
        res,
        Err(maptile_core::error::MaptileError::Config(_))
    ));
}

// installs_tile_function_in_function_schema test removed —
// PostgresTableSource no longer creates plpgsql tile functions
// (spec D5/D6 inline filter cutover).

#[tokio::test]
async fn serves_basic_tile() {
    let dsn = match common::postgres::pg_dsn() {
        Some(d) => d,
        None => return,
    };
    let g = common::postgres::SchemaGuard::create(&dsn).await;
    create_simple_table(&dsn, &g.name, "t").await;
    let src = PostgresTableSource::open(make_pool(&dsn).await, cfg_for(&g.name, "t"))
        .await
        .unwrap();
    // Use a zoom above the advertised derived minimum for this small dataset.
    let resp = src
        .get_tile(
            TileCoord {
                z: 8,
                x: 128,
                y: 127,
            },
            None,
        )
        .await
        .unwrap();
    assert!(!resp.data.is_empty());
}

#[tokio::test]
async fn supports_filter_but_rejects_generic_url_query() {
    let dsn = match common::postgres::pg_dsn() {
        Some(d) => d,
        None => return,
    };
    let g = common::postgres::SchemaGuard::create(&dsn).await;
    create_simple_table(&dsn, &g.name, "t").await;
    let src = PostgresTableSource::open(make_pool(&dsn).await, cfg_for(&g.name, "t"))
        .await
        .unwrap();
    assert!(!src.supports_url_query());
    assert!(src.supports_filter());
}

#[tokio::test]
async fn bounds_override_takes_precedence_over_auto() {
    let dsn = match common::postgres::pg_dsn() {
        Some(d) => d,
        None => return,
    };
    let g = common::postgres::SchemaGuard::create(&dsn).await;
    create_simple_table(&dsn, &g.name, "t").await;
    let mut cfg = cfg_for(&g.name, "t");
    cfg.bounds = Some([-180.0, -85.0, 180.0, 85.0]);
    let src = PostgresTableSource::open(make_pool(&dsn).await, cfg)
        .await
        .unwrap();
    let bounds = src.tilejson().bounds.expect("bounds in tilejson");
    assert_eq!(bounds.left, -180.0);
    assert_eq!(bounds.right, 180.0);
}

// drop_function_on_close test removed — PostgresTableSource no longer
// installs a per-source plpgsql tile function (spec D5/D6 single-shot
// cutover to inline filter SQL).

#[tokio::test]
async fn clone_source_serves_tiles() {
    let dsn = match common::postgres::pg_dsn() {
        Some(d) => d,
        None => return,
    };
    let g = common::postgres::SchemaGuard::create(&dsn).await;
    create_simple_table(&dsn, &g.name, "t").await;
    let src = PostgresTableSource::open(make_pool(&dsn).await, cfg_for(&g.name, "t"))
        .await
        .unwrap();
    // Use a zoom above the advertised derived minimum for this small dataset.
    let cloned = src.clone_source();
    let r1 = src
        .get_tile(
            TileCoord {
                z: 8,
                x: 128,
                y: 127,
            },
            None,
        )
        .await
        .unwrap();
    let r2 = cloned
        .get_tile(
            TileCoord {
                z: 8,
                x: 128,
                y: 127,
            },
            None,
        )
        .await
        .unwrap();
    assert!(!r1.data.is_empty());
    assert!(!r2.data.is_empty());
}
