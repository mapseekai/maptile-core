//! Ported from `tileserver-rs` (`crates/tileserver-rs/src/render/overlay.rs`)
//! under the MIT license; see `third_party/tileserver-rs/LICENSE`.

use super::*;
use image::RgbaImage;

fn encode_polyline(points: &[GeoPoint]) -> String {
    let mut encoded = String::new();
    let mut prev_lat = 0i64;
    let mut prev_lng = 0i64;
    for point in points {
        let lat = (point.lat * 1e5).round() as i64;
        let lng = (point.lon * 1e5).round() as i64;
        encode_value(lat - prev_lat, &mut encoded);
        encode_value(lng - prev_lng, &mut encoded);
        prev_lat = lat;
        prev_lng = lng;
    }
    encoded
}

fn encode_value(mut value: i64, output: &mut String) {
    value = if value < 0 { !(value << 1) } else { value << 1 };
    while value >= 0x20 {
        output.push(((value & 0x1f) as u8 + 63 + 0x20) as char);
        value >>= 5;
    }
    output.push((value as u8 + 63) as char);
}

#[test]
fn malformed_overlay_strings_do_not_panic() {
    for path in ["path-red(", "path-(é", "path-", "path-3(0,0|1,1"] {
        assert!(parse_path(path, false).is_none(), "{path}");
    }
    for marker in ["pin-l(", "pin-(é", "pin-", "pin-l(0,0"] {
        assert!(parse_marker(marker).is_none(), "{marker}");
    }
    for color in ["éa", "éaa", "éaaaa", "éaaaaaa", "中", "💩"] {
        assert!(parse_hex_color(color).is_none());
        let path = parse_path(&format!("path-3+{color}(0,0|1,1)"), false).unwrap();
        assert_eq!(path.stroke_color, Rgba([0, 0, 255, 255]));
        let marker = parse_marker(&format!("pin-l+{color}(0,0)")).unwrap();
        assert_eq!(marker.color, Rgba([255, 0, 0, 255]));
    }
}

#[test]
fn invalid_polyline_stops_at_complete_prefix_and_checks_integer_bounds() {
    for invalid in [
        "~".repeat(100_000),
        "é".into(),
        "!".into(),
        "_".into(),
        "?".into(),
    ] {
        assert!(decode_polyline(&invalid).is_empty());
        let points = decode_polyline(&format!("??{invalid}"));
        assert_eq!(points.len(), 1);
        assert_eq!((points[0].lon, points[0].lat), (0.0, 0.0));
    }
    // Twelve continuation groups plus a final nibble fill u64 exactly.
    let max_delta = format!("}}{}N", "~".repeat(11));
    let min_delta = format!("{}N", "~".repeat(12));
    assert_eq!(
        decode_polyline(&format!("{max_delta}?{max_delta}?")).len(),
        1
    );
    assert_eq!(
        decode_polyline(&format!("{min_delta}?{min_delta}?")).len(),
        1
    );
    // A thirteenth group with five payload bits cannot fit u64.
    assert!(decode_polyline(&format!("{}^?", "~".repeat(12))).is_empty());
    assert!(parse_path(&format!("enc:{}", "~".repeat(100_000)), false).is_none());
}

#[test]
fn parse_hex_color_3_digit() {
    assert_eq!(parse_hex_color("f00"), Some(Rgba([255, 0, 0, 255])));
    assert_eq!(parse_hex_color("abc"), Some(Rgba([170, 187, 204, 255])));
}

#[test]
fn parse_hex_color_6_and_8_digit() {
    assert_eq!(parse_hex_color("ff0000"), Some(Rgba([255, 0, 0, 255])));
    assert_eq!(parse_hex_color("ff000080"), Some(Rgba([255, 0, 0, 128])));
    assert_eq!(parse_hex_color("#aabbcc"), Some(Rgba([170, 187, 204, 255])));
}

#[test]
fn parse_hex_color_invalid_lengths() {
    assert_eq!(parse_hex_color(""), None);
    assert_eq!(parse_hex_color("ffffff0"), None);
}

#[test]
fn polyline_round_trip_wikipedia_example() {
    // Wikipedia's worked example: 38.5,-120.2 / 40.7,-120.95 / 43.252,-126.453.
    let pts = decode_polyline("_p~iF~ps|U_ulLnnqC_mqNvxq`@");
    assert_eq!(pts.len(), 3);
    assert!((pts[0].lat - 38.5).abs() < 1e-5);
    assert!((pts[0].lon - (-120.2)).abs() < 1e-5);
    assert!((pts[2].lat - 43.252).abs() < 1e-5);
    assert!((pts[2].lon - (-126.453)).abs() < 1e-5);

    // Round-trip through the encoder.
    let re = encode_polyline(&pts);
    assert_eq!(re, "_p~iF~ps|U_ulLnnqC_mqNvxq`@");
}

#[test]
fn parse_path_basic_query() {
    let p = parse_path("path-5+f00(-122.4,37.8|-122.5,37.9)", false).unwrap();
    assert_eq!(p.points.len(), 2);
    assert_eq!(p.stroke_width, 5.0);
    assert_eq!(p.stroke_color, Rgba([255, 0, 0, 255]));
}

#[test]
fn parse_path_with_fill_color() {
    let p = parse_path("path-3+00f-ff0(-10,20|30,40|50,60)", false).unwrap();
    assert_eq!(p.points.len(), 3);
    assert_eq!(p.fill_color, Some(Rgba([255, 255, 0, 255])));
}

#[test]
fn parse_path_single_point_returns_none() {
    assert!(parse_path("path-5+f00(-122.4,37.8)", false).is_none());
    assert!(parse_path("0,0", false).is_none());
}

#[test]
fn parse_marker_pin_sizes() {
    assert_eq!(parse_marker("pin-s+fff(0,0)").unwrap().size, 20.0);
    assert_eq!(parse_marker("pin-m+fff(0,0)").unwrap().size, 28.0);
    assert_eq!(parse_marker("pin-l+fff(0,0)").unwrap().size, 36.0);
    assert_eq!(parse_marker("pin-x+fff(0,0)").unwrap().size, 24.0);
}

#[test]
fn parse_marker_simple_lonlat() {
    let m = parse_marker("-122.4,37.8").unwrap();
    assert!((m.position.lon - (-122.4)).abs() < 1e-6);
    assert!((m.position.lat - 37.8).abs() < 1e-6);
    assert_eq!(m.color, Rgba([255, 0, 0, 255]));
}

#[test]
fn parse_marker_invalid_returns_none() {
    assert!(parse_marker("").is_none());
    assert!(parse_marker("pin-s+f00()").is_none());
    assert!(parse_marker("invalid").is_none());
}

#[test]
fn calculate_bounds_combines_paths_and_markers() {
    let paths = vec![PathOverlay {
        points: vec![
            GeoPoint { lon: 0.0, lat: 0.0 },
            GeoPoint {
                lon: 10.0,
                lat: 5.0,
            },
        ],
        stroke_color: Rgba([0, 0, 255, 255]),
        stroke_width: 3.0,
        fill_color: None,
    }];
    let markers = vec![MarkerOverlay {
        position: GeoPoint {
            lon: -1.0,
            lat: 6.0,
        },
        color: Rgba([255, 0, 0, 255]),
        size: 24.0,
    }];
    let (min_lon, min_lat, max_lon, max_lat) = calculate_bounds(&paths, &markers).unwrap();
    assert!((min_lon - (-1.0)).abs() < 1e-9);
    assert!((min_lat - 0.0).abs() < 1e-9);
    assert!((max_lon - 10.0).abs() < 1e-9);
    assert!((max_lat - 6.0).abs() < 1e-9);
}

#[test]
fn calculate_bounds_empty_returns_none() {
    assert!(calculate_bounds(&[], &[]).is_none());
}

#[test]
fn parse_geojson_feature_collection_lines_and_points() {
    let json = r##"{
        "type": "FeatureCollection",
        "features": [
            {"type": "Feature", "properties": {"stroke":"#ff0000","stroke-width":2.0},
             "geometry": {"type": "LineString", "coordinates": [[0,0],[1,1]]}},
            {"type": "Feature", "properties": {"marker-color":"#00ff00","marker-size":"large"},
             "geometry": {"type": "Point", "coordinates": [5,5]}}
        ]
    }"##;
    let (paths, markers) = parse_geojson(json);
    assert_eq!(paths.len(), 1);
    assert_eq!(paths[0].stroke_color, Rgba([255, 0, 0, 255]));
    assert_eq!(paths[0].stroke_width, 2.0);
    assert_eq!(markers.len(), 1);
    assert_eq!(markers[0].color, Rgba([0, 255, 0, 255]));
    assert_eq!(markers[0].size, 18.0);
}

#[test]
fn parse_geojson_polygon_outer_ring_only() {
    let json = r#"{"type": "Polygon", "coordinates": [
        [[0,0],[10,0],[10,10],[0,10],[0,0]],
        [[2,2],[8,2],[8,8],[2,8],[2,2]]
    ]}"#;
    let (paths, _) = parse_geojson(json);
    assert_eq!(paths.len(), 1);
    assert_eq!(paths[0].points.len(), 5);
}

#[test]
fn parse_geojson_invalid_json_returns_empty() {
    let (paths, markers) = parse_geojson("not json");
    assert!(paths.is_empty());
    assert!(markers.is_empty());
}

#[test]
fn draw_overlays_paints_path_pixels() {
    // Render a 2-pixel-wide red line from (-1, 0) to (1, 0) on a
    // 10x10 white canvas centered on (0, 0) at zoom 0. The path
    // crosses the image horizontally near y=5.
    let mut img = RgbaImage::from_pixel(10, 10, Rgba([255, 255, 255, 255]));
    let path = PathOverlay {
        points: vec![
            GeoPoint {
                lon: -1.0,
                lat: 0.0,
            },
            GeoPoint { lon: 1.0, lat: 0.0 },
        ],
        stroke_color: Rgba([255, 0, 0, 255]),
        stroke_width: 2.0,
        fill_color: None,
    };
    // Zoom high enough that the lon span fills the canvas.
    draw_overlays(&mut img, &[path], &[], 0.0, 0.0, 1.5, 1.0);
    let mut red = 0;
    for px in img.pixels() {
        if px.0[0] == 255 && px.0[1] == 0 && px.0[2] == 0 {
            red += 1;
        }
    }
    assert!(red > 0, "expected at least one red pixel from the stroke");
}

#[test]
fn draw_overlays_makes_transparent_canvas_visible() {
    let mut image = RgbaImage::from_pixel(32, 32, Rgba([0, 0, 0, 0]));
    let marker = MarkerOverlay {
        position: GeoPoint { lon: 0.0, lat: 0.0 },
        color: Rgba([255, 0, 0, 255]),
        size: 12.0,
    };

    draw_overlays(&mut image, &[], &[marker], 0.0, 0.0, 0.0, 1.0);

    assert!(image.pixels().any(|pixel| pixel.0[3] != 0));
}

#[test]
fn expand_overlays_routes_each_kind() {
    let items = vec![
        OverlayItem::Path(PathOverlay {
            points: vec![
                GeoPoint { lon: 0.0, lat: 0.0 },
                GeoPoint { lon: 1.0, lat: 1.0 },
            ],
            stroke_color: Rgba([0, 0, 255, 255]),
            stroke_width: 1.0,
            fill_color: None,
        }),
        OverlayItem::Marker(MarkerOverlay {
            position: GeoPoint { lon: 2.0, lat: 2.0 },
            color: Rgba([255, 0, 0, 255]),
            size: 24.0,
        }),
        OverlayItem::GeoJson(
            r#"{"type":"Feature","geometry":{"type":"Point","coordinates":[3,3]}}"#.into(),
        ),
    ];
    let (paths, markers) = expand_overlays(&items);
    assert_eq!(paths.len(), 1);
    assert_eq!(markers.len(), 2);
}
