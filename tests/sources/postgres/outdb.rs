//! OutDB integration tests share the PostgreSQL function test target and fixtures.

use super::common;
use maptile_core::{
    raster::style::EffectiveStyle,
    sources::{
        TileCoord, TileQuery, TileSource,
        postgres::{
            PoolSettings, PostgresOutDbRasterConfig, PostgresOutDbRasterSource, PostgresPool,
        },
    },
};
use std::sync::Arc;

#[tokio::test]
async fn quoted_schema_and_function_render_real_raster() {
    let Some(dsn) = common::postgres::pg_dsn() else {
        return;
    };
    let guard = common::postgres::SchemaGuard::create_named(
        &dsn,
        format!("{}\"schema", common::postgres::unique_schema()),
    )
    .await;
    let pool = Arc::new(
        PostgresPool::connect(&dsn, PoolSettings::default())
            .await
            .unwrap(),
    );
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.tif");
    let extent = 20_037_508.342_789_244;
    let mut dataset = gdal::DriverManager::get_driver_by_name("GTiff")
        .unwrap()
        .create_with_band_type::<u8, _>(&path, 64, 64, 3)
        .unwrap();
    dataset
        .set_geo_transform(&[
            -extent,
            2.0 * extent / 64.0,
            0.0,
            extent,
            0.0,
            -2.0 * extent / 64.0,
        ])
        .unwrap();
    dataset
        .set_spatial_ref(&gdal::spatial_ref::SpatialRef::from_epsg(3857).unwrap())
        .unwrap();
    for (i, value) in [50.0, 100.0, 150.0].into_iter().enumerate() {
        dataset
            .rasterband(i + 1)
            .unwrap()
            .fill(value, None)
            .unwrap();
    }
    dataset.flush_cache().unwrap();
    drop(dataset);
    let name = "raster\"function";
    let schema = pg_escape::quote_identifier(&guard.name);
    let function = pg_escape::quote_identifier(name);
    let filepath = pg_escape::quote_literal(path.to_str().unwrap());
    pool.get().await.unwrap().batch_execute(&format!(r#"
        CREATE FUNCTION {schema}.{function}(z integer,x integer,y integer,bounds geometry,params jsonb)
        RETURNS TABLE(filepath text) LANGUAGE sql AS $$
        SELECT {filepath}::text WHERE z=0 AND x=0 AND y=0 AND ST_XMin(bounds)<0 AND params='{{}}'::jsonb
        UNION ALL SELECT repeat('x', 16385) WHERE z=1 AND x=0 AND y=0
        UNION ALL SELECT {filepath}::text FROM generate_series(1, 65) WHERE z=1 AND x=1 AND y=0;
        $$;
    "#)).await.unwrap();
    let config: PostgresOutDbRasterConfig = serde_json::from_value(serde_json::json!({
        "id":"quoted-outdb","schema":guard.name,"function":name
    }))
    .unwrap();
    let source = PostgresOutDbRasterSource::new(pool.clone(), &config)
        .await
        .unwrap();
    pool.get()
        .await
        .unwrap()
        .prepare(source.tile_query())
        .await
        .expect("quoted names prepare");
    let style=EffectiveStyle::from_raster_style(raster_style::Style::from_value(serde_json::json!({
        "extensions":["https://mapseekai.github.io/raster-style/extensions/raster/tile/v1.0.0/schema.json"],
        "version":"3.0","renderer":{"type":"rgb","bidx":[1,2,3]},
        "raster:tile":{"format":"png","tilesize":64}
    })).unwrap()).unwrap();
    let coord = TileCoord { z: 0, x: 0, y: 0 };
    let query = TileQuery {
        style: Some(&style),
        ..Default::default()
    };
    let tile = source.get_tile(coord, Some(&query)).await.unwrap();
    let image = image::load_from_memory(&tile.data).unwrap().to_rgba8();
    assert_eq!(image.dimensions(), (64, 64));
    assert_eq!(image.get_pixel(32, 32).0, [50, 100, 150, 255]);
    let batch = source
        .read_tile_batch(&[coord], Some(&query))
        .await
        .unwrap();
    assert_eq!(batch[0].data, tile.data);
    for x in [0, 1] {
        assert!(matches!(
            source
                .get_tile(TileCoord { z: 1, x, y: 0 }, Some(&query))
                .await,
            Err(maptile_core::error::MaptileError::Resource(_))
        ));
    }
    common::postgres::assert_invalid_tile_requests(&source, Some(&style)).await;
    for coord in [
        TileCoord {
            z: 31,
            x: 1 << 31,
            y: 0,
        },
        TileCoord { z: 1, x: 10, y: 10 },
    ] {
        let error = source
            .get_tile_with_params(coord.z, coord.x, coord.y, None, &style)
            .await
            .unwrap_err();
        assert!(
            matches!(error, maptile_core::error::MaptileError::InvalidRequest(_)),
            "{error:?}"
        );
    }
}
