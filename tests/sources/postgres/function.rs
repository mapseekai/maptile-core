//! Tests for `PostgresFunctionSource` — require `DATABASE_URL`.

#[path = "../../common/mod.rs"]
mod common;

use maptile_core::sources::postgres::{
    PoolSettings, PostgresFunctionConfig, PostgresFunctionSource, PostgresPool,
};
use maptile_core::sources::{TileCoord, TileFormat, TileSource};
use std::sync::Arc;

async fn make_pool(dsn: &str) -> Arc<PostgresPool> {
    Arc::new(
        PostgresPool::connect(dsn, PoolSettings::default())
            .await
            .expect("connect"),
    )
}

async fn install_basic_function(dsn: &str, schema: &str, fn_name: &str) {
    let (client, conn) = tokio_postgres::connect(dsn, tokio_postgres::NoTls)
        .await
        .expect("connect for setup");
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let sql = format!(
        r#"
        CREATE OR REPLACE FUNCTION {schema}.{fn_name}(z int, x int, y int)
        RETURNS bytea AS $$
        DECLARE
            mvt bytea;
        BEGIN
            SELECT ST_AsMVT(tile, 'demo')
            INTO mvt
            FROM (
                SELECT 'hello' AS label,
                       ST_AsMVTGeom(
                           ST_GeomFromText('POINT(0 0)', 3857),
                           ST_TileEnvelope(z, x, y),
                           4096, 64, true
                       ) AS geom
            ) AS tile;
            RETURN mvt;
        END;
        $$ LANGUAGE plpgsql;
        "#,
        schema = pg_escape::quote_identifier(schema),
        fn_name = pg_escape::quote_identifier(fn_name)
    );
    client.batch_execute(&sql).await.expect("install function");
}

#[tokio::test]
async fn function_source_serves_basic_tile() {
    let dsn = match common::postgres::pg_dsn() {
        Some(d) => d,
        None => return,
    };
    let guard = common::postgres::SchemaGuard::create(&dsn).await;
    install_basic_function(&dsn, &guard.name, "tiles").await;

    let pool = make_pool(&dsn).await;
    let src = PostgresFunctionSource::open(
        pool,
        PostgresFunctionConfig {
            id: "demo".into(),
            schema: guard.name.clone(),
            function: "tiles".into(),
            description: None,
            minzoom: Some(0),
            maxzoom: Some(14),
            bounds: None,
            cache_zoom: None,
        },
    )
    .await
    .unwrap();

    assert_eq!(src.tile_info().format, TileFormat::Mvt);
    assert!(!src.supports_url_query());

    let resp = src
        .get_tile(TileCoord { z: 0, x: 0, y: 0 }, None)
        .await
        .unwrap();
    assert!(!resp.data.is_empty(), "expected MVT bytes");
}

async fn install_with_query_function(dsn: &str, schema: &str, fn_name: &str) {
    let (client, conn) = tokio_postgres::connect(dsn, tokio_postgres::NoTls)
        .await
        .expect("connect for setup");
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let sql = format!(
        r#"
        CREATE OR REPLACE FUNCTION "{schema}"."{fn_name}"(z int, x int, y int, query json)
        RETURNS bytea AS $$
        DECLARE
            mvt bytea;
            label text := COALESCE(query->>'label', 'default');
        BEGIN
            SELECT ST_AsMVT(tile, label)
            INTO mvt
            FROM (
                SELECT 1 AS k,
                       ST_AsMVTGeom(
                           ST_GeomFromText('POINT(0 0)', 3857),
                           ST_TileEnvelope(z, x, y),
                           4096, 64, true
                       ) AS geom
            ) AS tile;
            RETURN mvt;
        END;
        $$ LANGUAGE plpgsql;
        "#
    );
    client.batch_execute(&sql).await.expect("install function");
}

#[tokio::test]
async fn supports_url_query_reflects_signature() {
    let dsn = match common::postgres::pg_dsn() {
        Some(d) => d,
        None => return,
    };
    let guard = common::postgres::SchemaGuard::create(&dsn).await;
    install_basic_function(&dsn, &guard.name, "basic").await;
    install_with_query_function(&dsn, &guard.name, "with_q").await;

    let pool = make_pool(&dsn).await;
    let basic = PostgresFunctionSource::open(
        pool.clone(),
        PostgresFunctionConfig {
            id: "b".into(),
            schema: guard.name.clone(),
            function: "basic".into(),
            description: None,
            minzoom: None,
            maxzoom: None,
            bounds: None,
            cache_zoom: None,
        },
    )
    .await
    .unwrap();

    let with_q = PostgresFunctionSource::open(
        pool,
        PostgresFunctionConfig {
            id: "q".into(),
            schema: guard.name.clone(),
            function: "with_q".into(),
            description: None,
            minzoom: None,
            maxzoom: None,
            bounds: None,
            cache_zoom: None,
        },
    )
    .await
    .unwrap();

    assert!(!basic.supports_url_query());
    assert!(with_q.supports_url_query());
}

#[tokio::test]
async fn function_edits_and_same_id_replacement_are_visible() {
    use maptile_core::{
        cache::TileCache,
        control::CatalogService,
        service::{ReadTarget, TileStack},
        sources::mvt,
    };

    let Some(dsn) = common::postgres::pg_dsn() else {
        return;
    };
    let guard = common::postgres::SchemaGuard::create(&dsn).await;
    install_basic_function(&dsn, &guard.name, "basic").await;
    install_with_query_function(&dsn, &guard.name, "json_tiles").await;
    let pool = make_pool(&dsn).await;
    pool.get()
        .await
        .unwrap()
        .batch_execute(&format!(
            "CREATE FUNCTION \"{0}\".jsonb_tiles(int, int, int, jsonb) RETURNS bytea
         LANGUAGE sql AS $$ SELECT \"{0}\".json_tiles($1,$2,$3,$4::json) $$",
            guard.name
        ))
        .await
        .unwrap();
    let mut cfg = PostgresFunctionConfig {
        id: "same".into(),
        schema: guard.name.clone(),
        function: "basic".into(),
        description: None,
        minzoom: None,
        maxzoom: None,
        bounds: None,
        cache_zoom: None,
    };
    let old = PostgresFunctionSource::open(pool.clone(), cfg.clone())
        .await
        .unwrap();
    let engine = Arc::new(CatalogService::new(TileCache::new(8, None, None).unwrap()));
    let stack = TileStack::builder(engine.clone()).build();
    let mut update = engine.catalog().update();
    update.put(Arc::new(old)).unwrap();
    engine.catalog().publish(update).unwrap();
    let before_snapshot = engine.catalog().snapshot();
    let before_revision = before_snapshot.describe("same").unwrap().revision;
    let request = ReadTarget::new("same", "public").at(TileCoord { z: 0, x: 0, y: 0 });
    let before = stack.read(request).await.unwrap();
    assert_eq!(
        mvt::decode(&before.tile().unwrap().data).unwrap().layers[0].name,
        "demo"
    );
    pool.get()
        .await
        .unwrap()
        .batch_execute(&format!(
            r#"
        CREATE OR REPLACE FUNCTION "{}".basic(z integer, x integer, y integer)
        RETURNS bytea LANGUAGE sql AS $$ SELECT ''::bytea $$;
    "#,
            guard.name
        ))
        .await
        .unwrap();
    assert!(
        stack
            .read(request)
            .await
            .unwrap()
            .tile()
            .unwrap()
            .data
            .is_empty()
    );

    cfg.function = "jsonb_tiles".into();
    let new = PostgresFunctionSource::open(pool, cfg).await.unwrap();
    let mut update = engine.catalog().update();
    update.put(Arc::new(new)).unwrap();
    engine.catalog().publish(update).unwrap();
    let after_snapshot = engine.catalog().snapshot();
    let after = after_snapshot.describe("same").unwrap();
    assert_ne!(after.revision, before_revision);
    assert!(after.capabilities.url_query);
    let replacement = stack.read(request).await.unwrap();
    assert_eq!(
        mvt::decode(&replacement.tile().unwrap().data)
            .unwrap()
            .layers[0]
            .name,
        "default"
    );
}

#[tokio::test]
async fn function_source_passes_query_params() {
    use maptile_core::sources::TileQuery;
    use std::collections::BTreeMap;

    let dsn = match common::postgres::pg_dsn() {
        Some(d) => d,
        None => return,
    };
    let guard = common::postgres::SchemaGuard::create(&dsn).await;
    install_with_query_function(&dsn, &guard.name, "tiles").await;

    let pool = make_pool(&dsn).await;
    let src = PostgresFunctionSource::open(
        pool,
        PostgresFunctionConfig {
            id: "demo".into(),
            schema: guard.name.clone(),
            function: "tiles".into(),
            description: None,
            minzoom: None,
            maxzoom: None,
            bounds: None,
            cache_zoom: None,
        },
    )
    .await
    .unwrap();

    let mut q = BTreeMap::new();
    q.insert("label".to_string(), "custom".to_string());
    let tq = TileQuery {
        url_query: Some(&q),
        filter: None,
        bbox: None,
        style: None,
        style_hash: None,
        pixel_selection: None,
    };
    let resp = src
        .get_tile(TileCoord { z: 0, x: 0, y: 0 }, Some(&tq))
        .await
        .unwrap();
    assert!(
        resp.data.windows(b"custom".len()).any(|w| w == b"custom"),
        "expected layer name 'custom' in MVT bytes"
    );
    let batch = src
        .read_tile_batch(&[TileCoord { z: 0, x: 0, y: 0 }; 2], Some(&tq))
        .await
        .unwrap();
    assert_eq!(batch.len(), 2);
    assert!(batch.iter().all(|tile| tile.data == resp.data));
}

#[tokio::test]
async fn open_fails_on_missing_function() {
    let dsn = match common::postgres::pg_dsn() {
        Some(d) => d,
        None => return,
    };
    let guard = common::postgres::SchemaGuard::create(&dsn).await;
    let pool = make_pool(&dsn).await;
    let res = PostgresFunctionSource::open(
        pool,
        PostgresFunctionConfig {
            id: "x".into(),
            schema: guard.name.clone(),
            function: "nonexistent".into(),
            description: None,
            minzoom: None,
            maxzoom: None,
            bounds: None,
            cache_zoom: None,
        },
    )
    .await;
    assert!(matches!(
        res,
        Err(maptile_core::error::MaptileError::Config(_))
    ));
}

#[tokio::test]
async fn open_fails_on_unsupported_signature() {
    let dsn = match common::postgres::pg_dsn() {
        Some(d) => d,
        None => return,
    };
    let guard = common::postgres::SchemaGuard::create(&dsn).await;
    let (client, conn) = tokio_postgres::connect(&dsn, tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });
    client
        .batch_execute(&format!(
            r#"CREATE FUNCTION "{}"."bad"(x int) RETURNS bytea AS $$ SELECT NULL::bytea $$ LANGUAGE sql;"#,
            guard.name
        ))
        .await
        .unwrap();

    let pool = make_pool(&dsn).await;
    let res = PostgresFunctionSource::open(
        pool,
        PostgresFunctionConfig {
            id: "x".into(),
            schema: guard.name.clone(),
            function: "bad".into(),
            description: None,
            minzoom: None,
            maxzoom: None,
            bounds: None,
            cache_zoom: None,
        },
    )
    .await;
    assert!(matches!(
        res,
        Err(maptile_core::error::MaptileError::Config(_))
    ));
}

#[tokio::test]
async fn null_bytea_means_missing_but_empty_bytea_is_valid() {
    let dsn = match common::postgres::pg_dsn() {
        Some(d) => d,
        None => return,
    };
    let guard = common::postgres::SchemaGuard::create(&dsn).await;
    let (client, conn) = tokio_postgres::connect(&dsn, tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });
    client
        .batch_execute(&format!(
            r#"CREATE FUNCTION "{}"."nulltile"(z int, x int, y int) RETURNS bytea AS $$ SELECT NULL::bytea $$ LANGUAGE sql;"#,
            guard.name
        ))
        .await
        .unwrap();

    let pool = make_pool(&dsn).await;
    let src = PostgresFunctionSource::open(
        pool,
        PostgresFunctionConfig {
            id: "n".into(),
            schema: guard.name.clone(),
            function: "nulltile".into(),
            description: None,
            minzoom: None,
            maxzoom: None,
            bounds: None,
            cache_zoom: None,
        },
    )
    .await
    .unwrap();

    let res = src.get_tile(TileCoord { z: 0, x: 0, y: 0 }, None).await;
    assert!(matches!(
        res,
        Err(maptile_core::error::MaptileError::TileNotFound(0, 0, 0))
    ));
    let missing = src
        .read_tile_batch(&[TileCoord { z: 0, x: 0, y: 0 }; 2], None)
        .await
        .unwrap();
    assert_eq!(missing.len(), 2);
    assert!(missing.iter().all(|tile| tile.data.is_empty()));
    client.batch_execute(&format!(
        r#"CREATE OR REPLACE FUNCTION "{}"."nulltile"(z int, x int, y int) RETURNS bytea AS $$ SELECT ''::bytea $$ LANGUAGE sql;"#,
        guard.name,
    )).await.unwrap();
    assert!(
        src.get_tile(TileCoord { z: 0, x: 0, y: 0 }, None)
            .await
            .unwrap()
            .data
            .is_empty()
    );
}

#[tokio::test]
async fn cloned_function_source_serves_same_tile_and_revision() {
    let dsn = match common::postgres::pg_dsn() {
        Some(d) => d,
        None => return,
    };
    let guard = common::postgres::SchemaGuard::create(&dsn).await;
    install_basic_function(&dsn, &guard.name, "tiles").await;

    let pool = make_pool(&dsn).await;
    let src = PostgresFunctionSource::open(
        pool,
        PostgresFunctionConfig {
            id: "demo".into(),
            schema: guard.name.clone(),
            function: "tiles".into(),
            description: None,
            minzoom: None,
            maxzoom: None,
            bounds: None,
            cache_zoom: None,
        },
    )
    .await
    .unwrap();

    let clone = src.clone();
    let r1 = src
        .get_tile(TileCoord { z: 0, x: 0, y: 0 }, None)
        .await
        .unwrap();
    let r2 = clone
        .get_tile(TileCoord { z: 0, x: 0, y: 0 }, None)
        .await
        .unwrap();
    assert!(!r1.data.is_empty());
    assert_eq!(r1.data, r2.data);
    assert_eq!(src.content_version(), clone.content_version());
}

#[tokio::test]
async fn batch_keeps_null_empty_order_and_resumes_its_byte_prefix() {
    use maptile_core::{
        cache::TileCache,
        control::CatalogService,
        error::MaptileError,
        service::{ReadTarget, TileOutcome, TileStack},
        transport::MAX_DECODED_TILE_BYTES,
    };

    let Some(dsn) = common::postgres::pg_dsn() else {
        return;
    };
    let guard = common::postgres::SchemaGuard::create(&dsn).await;
    let pool = make_pool(&dsn).await;
    pool.get().await.unwrap().batch_execute(&format!(r#"
      CREATE FUNCTION "{s}".batch_tiles(z integer,x integer,y integer) RETURNS bytea LANGUAGE sql AS $$
      SELECT CASE WHEN x=0 THEN NULL::bytea WHEN x=1 THEN ''::bytea
        WHEN x=3 AND y=1 THEN repeat('x', {MAX_DECODED_TILE_BYTES} + 1)::bytea ELSE
        (SELECT ST_AsMVT(tile,'points',4096,'geom','id') FROM (
          SELECT ST_AsMVTGeom(ST_Centroid(ST_TileEnvelope(z,x,y)),ST_TileEnvelope(z,x,y),4096,64,true) AS geom,
          (x*10+y)::bigint AS id,repeat('x',5*1024*1024) AS payload
        ) AS tile) END
      $$;
    "#,s=guard.name)).await.unwrap();
    let source = Arc::new(
        PostgresFunctionSource::open(
            pool,
            PostgresFunctionConfig {
                id: "batch-function".into(),
                schema: guard.name.clone(),
                function: "batch_tiles".into(),
                description: None,
                minzoom: None,
                maxzoom: None,
                bounds: None,
                cache_zoom: None,
            },
        )
        .await
        .unwrap(),
    );
    let coords = [
        TileCoord { z: 2, x: 2, y: 0 },
        TileCoord { z: 2, x: 0, y: 0 },
        TileCoord { z: 2, x: 1, y: 0 },
        TileCoord { z: 2, x: 3, y: 0 },
        TileCoord { z: 2, x: 2, y: 1 },
    ];
    let first = source.read_tile_batch(&coords, None).await.unwrap();
    assert_eq!(first.len(), 4);
    assert!(first[1].data.is_empty() && first[2].data.is_empty());
    let rest = source
        .read_tile_batch(&coords[first.len()..], None)
        .await
        .unwrap();
    assert_eq!(rest.len(), 1);
    for (coord, actual) in coords.into_iter().zip(first.into_iter().chain(rest)) {
        match source.get_tile(coord, None).await {
            Ok(single) => assert_eq!(actual.data, single.data),
            Err(maptile_core::error::MaptileError::TileNotFound(..)) => {
                assert!(actual.data.is_empty())
            }
            Err(error) => panic!("unexpected single read error: {error}"),
        }
    }
    assert!(source.read_tile_batch(&[], None).await.unwrap().is_empty());
    assert!(
        source
            .read_tile_batch(&[coords[0]; 129], None)
            .await
            .is_err()
    );

    let oversized = TileCoord { z: 2, x: 3, y: 1 };
    for error in [
        source.get_tile(oversized, None).await.unwrap_err(),
        source
            .read_tile_batch(&[oversized], None)
            .await
            .unwrap_err(),
    ] {
        assert!(matches!(
            error,
            MaptileError::PayloadTooLarge {
                limit: MAX_DECODED_TILE_BYTES
            }
        ));
    }

    let engine = Arc::new(CatalogService::new(TileCache::new(4, None, None).unwrap()));
    let mut update = engine.catalog().update();
    update.put(source).unwrap();
    engine.catalog().publish(update).unwrap();
    let stack = TileStack::builder(engine).build();
    let target = ReadTarget::new("batch-function", "public");
    let page = stack.read_batch(target, &coords).await.unwrap();
    assert_eq!(page.consumed, 4);
    assert!(
        page.tiles[0]
            .tile()
            .is_some_and(|tile| !tile.data.is_empty())
    );
    assert!(matches!(page.tiles[1], TileOutcome::NoTile));
    assert!(
        page.tiles[2]
            .tile()
            .is_some_and(|tile| tile.data.is_empty())
    );
    assert!(
        page.tiles[3]
            .tile()
            .is_some_and(|tile| !tile.data.is_empty())
    );
    assert!(matches!(
        stack.begin_export(target),
        Err(MaptileError::Unsupported(_))
    ));
}

#[tokio::test]
async fn invalid_requests_are_rejected_before_function_sql() {
    let Some(dsn) = common::postgres::pg_dsn() else {
        return;
    };
    let guard = common::postgres::SchemaGuard::create(&dsn).await;
    for with_query in [false, true] {
        let name = if with_query { "with_query" } else { "plain" };
        if with_query {
            install_with_query_function(&dsn, &guard.name, name).await;
        } else {
            install_basic_function(&dsn, &guard.name, name).await;
        }
        let source = PostgresFunctionSource::open(
            make_pool(&dsn).await,
            PostgresFunctionConfig {
                id: name.into(),
                schema: guard.name.clone(),
                function: name.into(),
                description: None,
                minzoom: None,
                maxzoom: None,
                bounds: None,
                cache_zoom: None,
            },
        )
        .await
        .unwrap();
        common::postgres::assert_invalid_tile_requests(&source, None).await;
    }
}

#[tokio::test]
async fn catalog_revocation_waits_for_cancelled_postgres_statements() {
    use common::postgres::{CANCELLATION_WATCHDOG, CancellationGate};
    use maptile_core::{
        cache::TileCache,
        control::CatalogService,
        error::MaptileError,
        service::{ReadTarget, TileStack},
    };
    use tokio::time::{Instant, timeout};

    let Some(dsn) = common::postgres::pg_dsn() else {
        return;
    };
    let guard = common::postgres::SchemaGuard::create(&dsn).await;
    let monitor_pool = make_pool(&dsn).await;
    let monitor = monitor_pool.get().await.unwrap();
    let mut gate_owner = monitor_pool.get().await.unwrap();
    let mut gate = CancellationGate::start(&monitor, &dsn).await;
    let pool = make_pool(&gate.dsn).await;
    monitor
        .batch_execute(&format!(
            r#"
        CREATE FUNCTION "{schema}".hold_for_revocation(z integer, x integer, y integer)
        RETURNS bytea LANGUAGE plpgsql AS $$
        BEGIN
            IF x = 0 THEN
                PERFORM pg_advisory_xact_lock({lock_key}, 1);
            END IF;
            RETURN ''::bytea;
        END $$;
    "#,
            schema = guard.name,
            lock_key = gate.lock_key,
        ))
        .await
        .unwrap();
    let catalog = Arc::new(CatalogService::new(TileCache::new(4, None, None).unwrap()));
    let stack = TileStack::builder(Arc::clone(&catalog)).build();
    for (id, batch) in [("single", false), ("batch", true)] {
        let config = PostgresFunctionConfig {
            id: id.into(),
            schema: guard.name.clone(),
            function: "hold_for_revocation".into(),
            description: None,
            minzoom: None,
            maxzoom: None,
            bounds: None,
            cache_zoom: None,
        };
        let source = PostgresFunctionSource::open(pool.clone(), config.clone())
            .await
            .unwrap();
        let live = PostgresFunctionSource::open(
            pool.clone(),
            PostgresFunctionConfig {
                id: "unrelated".into(),
                ..config
            },
        )
        .await
        .unwrap();
        let mut update = catalog.catalog().update();
        update.put(Arc::new(source)).unwrap();
        update.put(Arc::new(live)).unwrap();
        catalog.catalog().publish(update).unwrap();
        let observed = catalog.catalog().snapshot();
        let locks = gate.hold(&mut gate_owner).await;

        let job = tokio::spawn({
            let stack = stack.clone();
            async move {
                let coord = TileCoord { z: 1, x: 0, y: 0 };
                let target = ReadTarget::new(id, "public");
                if batch {
                    stack.read_batch(target, &[coord]).await.map(|_| ())
                } else {
                    stack.read(target.at(coord)).await.map(|_| ())
                }
            }
        });
        let pid = gate.wait_for_query().await;
        let mut drain = Box::pin(catalog.catalog().revoke(
            &observed,
            id,
            Instant::now() + CANCELLATION_WATCHDOG,
        ));
        assert!(futures_util::poll!(drain.as_mut()).is_pending());
        let release = gate.pause_cancellation(pid).await;
        assert!(matches!(
            timeout(CANCELLATION_WATCHDOG, job).await.unwrap().unwrap(),
            Err(MaptileError::Cancelled)
        ));
        assert!(futures_util::poll!(drain.as_mut()).is_pending());
        assert!(matches!(
            catalog
                .catalog()
                .revoke(&observed, id, Instant::now())
                .await,
            Err(MaptileError::Deadline)
        ));
        let unrelated = ReadTarget::new("unrelated", "public").at(TileCoord { z: 1, x: 1, y: 0 });
        assert!(
            timeout(CANCELLATION_WATCHDOG, stack.read(unrelated))
                .await
                .unwrap()
                .is_ok()
        );

        release.send(()).unwrap();
        drain.await.unwrap();
        gate.assert_inactive(pid).await;
        locks.commit().await.unwrap();
    }
}

#[tokio::test]
async fn quoted_schema_and_function_names_execute() {
    let Some(dsn) = common::postgres::pg_dsn() else {
        return;
    };
    let guard = common::postgres::SchemaGuard::create_named(
        &dsn,
        format!("{}\"schema", common::postgres::unique_schema()),
    )
    .await;
    let name = "tile\"function";
    install_basic_function(&dsn, &guard.name, name).await;
    let source = PostgresFunctionSource::open(
        make_pool(&dsn).await,
        PostgresFunctionConfig {
            id: name.into(),
            schema: guard.name.clone(),
            function: name.into(),
            description: None,
            minzoom: None,
            maxzoom: None,
            bounds: None,
            cache_zoom: None,
        },
    )
    .await
    .unwrap();
    let coord = TileCoord { z: 0, x: 0, y: 0 };
    let tile = source.get_tile(coord, None).await.unwrap();
    assert_eq!(
        maptile_core::sources::mvt::decode(&tile.data)
            .unwrap()
            .layers[0]
            .features
            .len(),
        1
    );
    assert_eq!(
        source.read_tile_batch(&[coord], None).await.unwrap()[0].data,
        tile.data
    );
}

#[cfg(feature = "raster")]
#[path = "outdb.rs"]
mod outdb;
