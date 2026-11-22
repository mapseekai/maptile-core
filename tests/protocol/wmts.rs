//! WMTS GetCapabilities golden snapshot.
//!
//! Captures the byte exact output of `build_capabilities` for a realistic
//! input (two layers — one with abstract+bbox, one minimal) so that the
//! quick-xml refactor can be verified byte-for-byte (or, if pretty-print
//! whitespace shifts, validated via event-stream equivalence).

use std::path::PathBuf;

use maptile_core::render::wmts::{WmtsLayer, WmtsService, build_capabilities};
use quick_xml::Reader;
use quick_xml::events::Event;

fn fixture_path() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/fixtures/wmts/golden.xml");
    p
}

fn sample_input() -> (WmtsService, Vec<WmtsLayer>) {
    let svc = WmtsService {
        title: "Maptile WMTS".into(),
        abstract_: Some("Self-hosted tile server with <special> chars & ampersands".into()),
        service_url: "https://example.com/styles/bright/wmts.xml?token=abc&v=1".into(),
    };
    let layers = vec![
        WmtsLayer {
            identifier: "bright".into(),
            title: "OSM Bright".into(),
            abstract_: Some("Light style with bbox".into()),
            tile_url_template:
                "https://example.com/styles/bright/{TileMatrix}/{TileCol}/{TileRow}.png".into(),
            format: "image/png".into(),
            bounding_box: Some([-180.0, -85.0, 180.0, 85.0]),
            min_zoom: 0,
            max_zoom: 14,
        },
        WmtsLayer {
            identifier: "dark".into(),
            title: "OSM Dark".into(),
            abstract_: None,
            tile_url_template:
                "https://example.com/styles/dark/{TileMatrix}/{TileCol}/{TileRow}.jpg".into(),
            format: "image/jpeg".into(),
            bounding_box: None,
            min_zoom: 0,
            max_zoom: 18,
        },
    ];
    (svc, layers)
}

/// Parse fully — verifies the document is well-formed XML.
fn assert_parses(xml: &str) {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => panic!("XML parse error at pos {}: {e}", reader.buffer_position()),
        }
        buf.clear();
    }
}

#[test]
fn golden_matches_fixture() {
    let (svc, layers) = sample_input();
    let xml = build_capabilities(&svc, &layers);

    // Always validate the output is parseable.
    assert_parses(&xml);

    let path = fixture_path();
    if std::env::var_os("WMTS_GOLDEN_REGEN").is_some() || !path.exists() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &xml).unwrap();
        eprintln!("wrote golden fixture: {}", path.display());
        return;
    }

    let expected = std::fs::read_to_string(&path).unwrap();
    if xml != expected {
        // Surface a small diff hint.
        let limit = xml.len().min(expected.len()).min(400);
        panic!(
            "golden mismatch.\n--- first {limit} of expected ---\n{}\n--- first {limit} of actual ---\n{}",
            &expected[..limit],
            &xml[..limit]
        );
    }
}

#[test]
fn output_has_expected_shape() {
    let (svc, layers) = sample_input();
    let xml = build_capabilities(&svc, &layers);
    assert!(xml.starts_with("<?xml"));
    assert!(xml.contains("<Capabilities"));
    assert!(xml.contains("</Capabilities>"));
    assert!(xml.contains("<ows:Identifier>bright</ows:Identifier>"));
    assert!(xml.contains("<ows:Identifier>dark</ows:Identifier>"));
    // Escaping check on service abstract.
    assert!(xml.contains("&lt;special&gt;"));
    assert!(xml.contains("&amp;"));
}
