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
        CREATE OR REPLACE FUNCTION "{schema}"."{fn_name}"(z int, x int, y int)
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
        "#
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
async fn clone_source_shares_pool() {
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

    let clone = src.clone_source();
    let r1 = src
        .get_tile(TileCoord { z: 0, x: 0, y: 0 }, None)
        .await
        .unwrap();
    let r2 = clone
        .get_tile(TileCoord { z: 0, x: 0, y: 0 }, None)
        .await
        .unwrap();
    assert!(!r1.data.is_empty());
    assert!(!r2.data.is_empty());
}

#[tokio::test]
async fn tile_etag_round_trip() {
    use maptile_core::sources::TileEtagResponse;

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

    let fresh = src
        .get_tile_with_etag(TileCoord { z: 0, x: 0, y: 0 }, None, None)
        .await
        .unwrap();
    let etag = match &fresh {
        TileEtagResponse::Fresh { etag, .. } => etag.clone(),
        TileEtagResponse::NotModified => panic!("expected Fresh on first request"),
    };

    let again = src
        .get_tile_with_etag(TileCoord { z: 0, x: 0, y: 0 }, None, Some(&etag))
        .await
        .unwrap();
    assert!(matches!(again, TileEtagResponse::NotModified));
}

#[tokio::test]
async fn batch_keeps_null_empty_order_and_resumes_its_byte_prefix() {
    let Some(dsn) = common::postgres::pg_dsn() else {
        return;
    };
    let guard = common::postgres::SchemaGuard::create(&dsn).await;
    let pool = make_pool(&dsn).await;
    pool.get().await.unwrap().batch_execute(&format!(r#"
      CREATE FUNCTION "{s}".batch_tiles(z integer,x integer,y integer) RETURNS bytea LANGUAGE sql AS $$
      SELECT CASE WHEN x=0 THEN NULL::bytea WHEN x=1 THEN ''::bytea ELSE
        (SELECT ST_AsMVT(tile,'points',4096,'geom','id') FROM (
          SELECT ST_AsMVTGeom(ST_Centroid(ST_TileEnvelope(z,x,y)),ST_TileEnvelope(z,x,y),4096,64,true) AS geom,
          (x*10+y)::bigint AS id,repeat('x',5*1024*1024) AS payload
        ) AS tile) END
      $$;
    "#,s=guard.name)).await.unwrap();
    let source = PostgresFunctionSource::open(
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
    .unwrap();
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
}
