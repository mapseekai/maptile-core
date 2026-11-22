use super::*;

#[test]
fn integer_family_maps_to_number() {
    assert_eq!(pg_type_to_tilejson_field("integer"), "Number");
    assert_eq!(pg_type_to_tilejson_field("bigint"), "Number");
    assert_eq!(pg_type_to_tilejson_field("smallint"), "Number");
    assert_eq!(pg_type_to_tilejson_field("real"), "Number");
    assert_eq!(pg_type_to_tilejson_field("double precision"), "Number");
    assert_eq!(pg_type_to_tilejson_field("numeric"), "Number");
}

#[test]
fn text_family_maps_to_string() {
    assert_eq!(pg_type_to_tilejson_field("text"), "String");
    assert_eq!(pg_type_to_tilejson_field("character varying"), "String");
    assert_eq!(pg_type_to_tilejson_field("character"), "String");
    assert_eq!(pg_type_to_tilejson_field("uuid"), "String");
}

#[test]
fn boolean_maps_to_boolean() {
    assert_eq!(pg_type_to_tilejson_field("boolean"), "Boolean");
}

#[test]
fn json_family_maps_to_string_per_tilejson_3_0() {
    // TileJSON 3.0 only allows String/Number/Boolean — JSON-typed columns
    // round-trip through MVT as opaque strings.
    assert_eq!(pg_type_to_tilejson_field("json"), "String");
    assert_eq!(pg_type_to_tilejson_field("jsonb"), "String");
}

#[test]
fn unknown_falls_back_to_string() {
    assert_eq!(pg_type_to_tilejson_field("inet"), "String");
    assert_eq!(pg_type_to_tilejson_field("bytea"), "String");
    assert_eq!(pg_type_to_tilejson_field("date"), "String");
}

#[test]
fn case_insensitive() {
    assert_eq!(pg_type_to_tilejson_field("INTEGER"), "Number");
    assert_eq!(pg_type_to_tilejson_field("Boolean"), "Boolean");
}

#[test]
fn global_bounds_derive_zoom_zero() {
    assert_eq!(derive_min_zoom(&[-180.0, -90.0, 180.0, 90.0]), 0);
}

#[test]
fn small_extent_derives_positive_zoom() {
    // 0.1° span: 4·360/(256·0.1) = 56.25 → ceil(log2) = 6.
    let z = derive_min_zoom(&[116.0, 39.0, 116.1, 39.1]);
    assert_eq!(z, 6);
    // At the derived zoom the dataset spans ≥ MIN_ZOOM_PIXELS px.
    let px = 0.1 / 360.0 * 256.0 * f64::from(1u32 << z);
    assert!(px >= MIN_ZOOM_PIXELS);
    // One zoom lower it would be sub-threshold.
    let px_below = px / 2.0;
    assert!(px_below < MIN_ZOOM_PIXELS);
}

#[test]
fn degenerate_bounds_derive_zero() {
    assert_eq!(derive_min_zoom(&[10.0, 20.0, 10.0, 20.0]), 0);
    // All-NaN bounds: span is NaN → defensive zero.
    assert_eq!(derive_min_zoom(&[f64::NAN; 4]), 0);
    // One NaN axis: f64::max ignores it, the finite axis wins.
    assert_eq!(derive_min_zoom(&[f64::NAN, 0.0, 1.0, 1.0]), 3);
}

#[test]
fn tiny_extent_clamps_to_22() {
    assert_eq!(derive_min_zoom(&[0.0, 0.0, 1e-12, 1e-12]), 22);
}

#[test]
fn tilejson_minzoom_set_from_derived_zoom() {
    let cfg = PostgresTableConfig::new("pts", "public", "pts");
    let bounds = [116.0, 39.0, 116.1, 39.1];
    let tj =
        build_tilejson_with_vector_layers(&cfg, Some(bounds), Some(derive_min_zoom(&bounds)), &[]);
    assert_eq!(tj.minzoom, Some(6));
    assert_eq!(tj.vector_layers.as_ref().unwrap()[0].minzoom, Some(6));
}

#[test]
fn tilejson_zoom_range_is_explicit_when_minimum_is_zero_or_unknown() {
    let cfg = PostgresTableConfig::new("pts", "public", "pts");
    let tj = build_tilejson_with_vector_layers(&cfg, None, Some(0), &[]);
    assert_eq!(tj.minzoom, Some(0));
    assert_eq!(tj.maxzoom, Some(22));
    let tj = build_tilejson_with_vector_layers(&cfg, None, None, &[]);
    assert_eq!(tj.minzoom, Some(0));
    assert_eq!(tj.maxzoom, Some(22));
}

#[test]
fn vector_layers_uses_source_id_and_typed_fields() {
    let cfg = PostgresTableConfig::new("roads", "public", "roads");
    let columns = vec![
        ("name".to_string(), "text".to_string()),
        ("highway".to_string(), "character varying".to_string()),
        ("lanes".to_string(), "integer".to_string()),
        ("oneway".to_string(), "boolean".to_string()),
    ];
    let tj = build_tilejson_with_vector_layers(&cfg, None, None, &columns);
    let layers = tj.vector_layers.as_ref().expect("vector_layers populated");
    assert_eq!(layers.len(), 1);
    let l = &layers[0];
    assert_eq!(l.id, "roads");
    assert_eq!(l.fields.get("name").map(String::as_str), Some("String"));
    assert_eq!(l.fields.get("highway").map(String::as_str), Some("String"));
    assert_eq!(l.fields.get("lanes").map(String::as_str), Some("Number"));
    assert_eq!(l.fields.get("oneway").map(String::as_str), Some("Boolean"));
}

#[test]
fn empty_columns_still_produce_one_layer() {
    let cfg = PostgresTableConfig::new("points", "public", "points");
    let tj = build_tilejson_with_vector_layers(&cfg, None, None, &[]);
    let layers = tj.vector_layers.as_ref().unwrap();
    assert_eq!(layers.len(), 1);
    assert_eq!(layers[0].id, "points");
    assert!(layers[0].fields.is_empty());
}

#[test]
fn description_propagates_to_layer() {
    let mut cfg = PostgresTableConfig::new("pts", "public", "pts");
    cfg.description = Some("hello".into());
    let tj = build_tilejson_with_vector_layers(&cfg, None, None, &[]);
    assert_eq!(tj.description.as_deref(), Some("hello"));
    assert_eq!(
        tj.vector_layers.as_ref().unwrap()[0].description.as_deref(),
        Some("hello")
    );
}

#[test]
fn bounds_are_set_when_provided() {
    let cfg = PostgresTableConfig::new("x", "public", "x");
    let tj = build_tilejson_with_vector_layers(&cfg, Some([-10.0, -5.0, 10.0, 5.0]), None, &[]);
    let b = tj.bounds.unwrap();
    assert!((b.left - -10.0).abs() < 1e-9);
    assert!((b.bottom - -5.0).abs() < 1e-9);
    assert!((b.right - 10.0).abs() < 1e-9);
    assert!((b.top - 5.0).abs() < 1e-9);
}
