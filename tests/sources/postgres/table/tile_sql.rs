use super::super::DEFAULT_EXTENT;
use super::*;

fn fixture_info() -> TableInfo {
    TableInfo {
        schema: "vector".into(),
        table: "test".into(),
        geometry_column: "geom".into(),
        geometry_type: "POINT".into(),
        srid: 4326,
        id_column: None,
        properties: Vec::new(),
        bounds: None,
        has_spatial_index: true,
        derived_min_zoom: None,
        extent: DEFAULT_EXTENT,
    }
}

#[test]
fn unfiltered_sql_includes_spatial_predicate() {
    let info = fixture_info();
    let sql = TileSql::new(&info, &[], true, SINGLE_COORDS).build(&info, "", None);
    assert!(sql.contains("ST_AsMVT"));
    assert!(sql.contains(" && "));
}

#[test]
fn batch_sql_uses_ordered_arrays_without_rewriting_quoted_identifiers() {
    let mut info = fixture_info();
    info.geometry_column = "$1::integer".into();
    let template = TileSql::new(
        &info,
        &[("$2::integer".into(), "text".into())],
        true,
        BATCH_COORDS,
    );
    let sql = compose_batch_sql(&template.build(&info, "batch_coords.enabled", None));
    assert!(sql.contains("unnest($1::integer[], $2::integer[], $3::integer[], $4::boolean[])"));
    assert!(sql.contains("WITH ORDINALITY"));
    assert!(sql.contains("ST_TileEnvelope(batch_coords.z, batch_coords.x, batch_coords.y)"));
    assert!(sql.contains("\"$1::integer\"::geometry"));
    assert!(sql.contains(", \"$2::integer\""));
    assert!(sql.ends_with("ORDER BY batch_tiles.ordinality"));
}

#[test]
fn compose_filtered_includes_filter_and_bbox() {
    let info = fixture_info();
    let sql = TileSql::new(&info, &[], true, SINGLE_COORDS).build(
        &info,
        "(\"class\" = 'highway')",
        Some([10.0, 20.0, 30.0, 40.0]),
    );
    assert!(sql.contains("ST_AsMVT"));
    assert!(sql.contains("\"class\" = 'highway'"));
    assert!(sql.contains("ST_MakeEnvelope(10, 20, 30, 40, 4326)"));
}

#[test]
fn compose_with_only_filter_omits_user_bbox_clause() {
    let info = fixture_info();
    let sql =
        TileSql::new(&info, &[], true, SINGLE_COORDS).build(&info, "(\"class\" = 'highway')", None);
    assert!(sql.contains("\"class\" = 'highway'"));
    assert!(!sql.contains("ST_MakeEnvelope"));
    assert!(sql.contains(" && "));
}

#[test]
fn compose_with_only_bbox_omits_filter_clause() {
    let info = fixture_info();
    let sql = TileSql::new(&info, &[], true, SINGLE_COORDS).build(
        &info,
        "",
        Some([10.0, 20.0, 30.0, 40.0]),
    );
    assert!(sql.contains("ST_MakeEnvelope"));
    // No "()" empty filter group.
    assert!(!sql.contains("AND ()"));
}

#[test]
fn inline_sql_quotes_postgres_identifiers_and_literals() {
    let mut info = fixture_info();
    info.schema = r#"tenant"1"#.into();
    info.table = "select".into();
    info.geometry_column = r#"geom"col"#.into();
    info.id_column = Some(r#"id"col"#.into());
    let columns = vec![(r#"name"col"#.to_string(), "text".to_string())];

    let sql = TileSql::new(&info, &columns, true, SINGLE_COORDS).build(&info, "", None);

    assert!(sql.contains(r#""tenant""1"."select""#));
    assert!(sql.contains(r#"ST_Transform("geom""col"::geometry, 3857)"#));
    assert!(sql.contains(r#""id""col"::bigint AS "id""col""#));
    assert!(sql.contains(r#""name""col""#));
    assert!(sql.contains(r#"ST_AsMVT(tile.*, 'select', 4096, 'geom"col', 'id"col')"#));
}

#[test]
fn filtered_sql_quotes_geometry_identifier() {
    let mut info = fixture_info();
    info.geometry_column = r#"geom"col"#.into();
    let sql = TileSql::new(&info, &[], true, SINGLE_COORDS).build(
        &info,
        "",
        Some([10.0, 20.0, 30.0, 40.0]),
    );

    assert!(sql.contains(r#""geom""col" && ST_Expand"#));
    assert!(sql.contains(r#""geom""col" && ST_MakeEnvelope(10, 20, 30, 40, 4326)"#));
}
