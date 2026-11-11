mod wkb_mvt_corpus {
    //! Golden corpus + parity tests for the WKB→MVT encoder.
    //!
    //! Builds an in-memory WKB corpus covering Point / LineString / Polygon /
    //! MultiPoint / MultiLineString / MultiPolygon, encodes each through the
    //! current public `encode_features_to_mvt` function, decodes via
    //! [`mvt::decode`], and asserts the resulting `GeomType` plus rough
    //! geometry shape (vertex/command counts).
    //!
    //! These tests are intentionally implementation-agnostic — they assert
    //! observable MVT properties, not byte equality. They must continue to
    //! pass after the geozero migration.

    use bytes::Bytes;
    use maptile_core::sources::mvt;
    use maptile_core::wkb_mvt::{WkbFeature, encode_features_to_mvt, wkb_bbox};

    // --- WKB builders (little-endian, OGC SFA) -----------------------------

    fn wkb_point(x: f64, y: f64) -> Bytes {
        let mut v = Vec::with_capacity(1 + 4 + 16);
        v.push(0x01);
        v.extend_from_slice(&1u32.to_le_bytes());
        v.extend_from_slice(&x.to_le_bytes());
        v.extend_from_slice(&y.to_le_bytes());
        Bytes::from(v)
    }

    fn wkb_linestring(pts: &[(f64, f64)]) -> Bytes {
        let mut v = Vec::new();
        v.push(0x01);
        v.extend_from_slice(&2u32.to_le_bytes());
        v.extend_from_slice(&(pts.len() as u32).to_le_bytes());
        for (x, y) in pts {
            v.extend_from_slice(&x.to_le_bytes());
            v.extend_from_slice(&y.to_le_bytes());
        }
        Bytes::from(v)
    }

    fn wkb_polygon(rings: &[&[(f64, f64)]]) -> Bytes {
        let mut v = Vec::new();
        v.push(0x01);
        v.extend_from_slice(&3u32.to_le_bytes());
        v.extend_from_slice(&(rings.len() as u32).to_le_bytes());
        for ring in rings {
            v.extend_from_slice(&(ring.len() as u32).to_le_bytes());
            for (x, y) in *ring {
                v.extend_from_slice(&x.to_le_bytes());
                v.extend_from_slice(&y.to_le_bytes());
            }
        }
        Bytes::from(v)
    }

    fn wkb_multipoint(pts: &[(f64, f64)]) -> Bytes {
        let mut v = Vec::new();
        v.push(0x01);
        v.extend_from_slice(&4u32.to_le_bytes());
        v.extend_from_slice(&(pts.len() as u32).to_le_bytes());
        for (x, y) in pts {
            // child Point header
            v.push(0x01);
            v.extend_from_slice(&1u32.to_le_bytes());
            v.extend_from_slice(&x.to_le_bytes());
            v.extend_from_slice(&y.to_le_bytes());
        }
        Bytes::from(v)
    }

    fn wkb_multilinestring(lines: &[&[(f64, f64)]]) -> Bytes {
        let mut v = Vec::new();
        v.push(0x01);
        v.extend_from_slice(&5u32.to_le_bytes());
        v.extend_from_slice(&(lines.len() as u32).to_le_bytes());
        for line in lines {
            v.push(0x01);
            v.extend_from_slice(&2u32.to_le_bytes());
            v.extend_from_slice(&(line.len() as u32).to_le_bytes());
            for (x, y) in *line {
                v.extend_from_slice(&x.to_le_bytes());
                v.extend_from_slice(&y.to_le_bytes());
            }
        }
        Bytes::from(v)
    }

    fn wkb_multipolygon(polys: &[&[&[(f64, f64)]]]) -> Bytes {
        let mut v = Vec::new();
        v.push(0x01);
        v.extend_from_slice(&6u32.to_le_bytes());
        v.extend_from_slice(&(polys.len() as u32).to_le_bytes());
        for rings in polys {
            v.push(0x01);
            v.extend_from_slice(&3u32.to_le_bytes());
            v.extend_from_slice(&(rings.len() as u32).to_le_bytes());
            for ring in *rings {
                v.extend_from_slice(&(ring.len() as u32).to_le_bytes());
                for (x, y) in *ring {
                    v.extend_from_slice(&x.to_le_bytes());
                    v.extend_from_slice(&y.to_le_bytes());
                }
            }
        }
        Bytes::from(v)
    }

    // Tile envelope used across this corpus: world-ish, fits the test
    // coordinates without clipping.
    const ENV: (f64, f64, f64, f64) = (-180.0, -90.0, 180.0, 90.0);
    const EXTENT: u32 = 4096;

    fn encode_one(wkb: Bytes) -> mvt::proto::Tile {
        let bytes = encode_features_to_mvt(
            "L",
            std::iter::once(WkbFeature {
                id: None,
                wkb,
                properties: serde_json::Map::new(),
            }),
            ENV,
            EXTENT,
        )
        .expect("encode succeeded");
        mvt::decode(&bytes).expect("decode succeeded")
    }

    #[test]
    fn corpus_point_encodes_to_point_geomtype() {
        let tile = encode_one(wkb_point(10.0, 20.0));
        assert_eq!(tile.layers[0].features.len(), 1);
        let f = &tile.layers[0].features[0];
        assert_eq!(f.r#type, Some(mvt::proto::GeomType::Point as i32));
        // single MoveTo(1) + dx + dy = 3 ints
        assert_eq!(f.geometry.len(), 3);
    }

    #[test]
    fn corpus_linestring_encodes_to_linestring_geomtype() {
        let tile = encode_one(wkb_linestring(&[(-10.0, 0.0), (10.0, 0.0), (10.0, 10.0)]));
        let f = &tile.layers[0].features[0];
        assert_eq!(f.r#type, Some(mvt::proto::GeomType::Linestring as i32));
        // Layout: MoveTo(1) + dx + dy + LineTo(2) + 2*(dx,dy) = 1 + 2 + 1 + 4 = 8 ints
        assert_eq!(f.geometry.len(), 8);
    }

    #[test]
    fn corpus_polygon_encodes_to_polygon_geomtype_with_close() {
        let ring: &[(f64, f64)] = &[
            (-10.0, -10.0),
            (10.0, -10.0),
            (10.0, 10.0),
            (-10.0, 10.0),
            (-10.0, -10.0),
        ];
        let tile = encode_one(wkb_polygon(&[ring]));
        let f = &tile.layers[0].features[0];
        assert_eq!(f.r#type, Some(mvt::proto::GeomType::Polygon as i32));
        // Last command is ClosePath: cmd=7, count=1 → (1<<3)|7 = 15
        assert_eq!(*f.geometry.last().unwrap(), 15);
    }

    #[test]
    fn corpus_multipoint_encodes_to_point_geomtype() {
        let tile = encode_one(wkb_multipoint(&[(0.0, 0.0), (10.0, 10.0), (20.0, -20.0)]));
        let f = &tile.layers[0].features[0];
        assert_eq!(f.r#type, Some(mvt::proto::GeomType::Point as i32));
        // MoveTo(3) + 3*(dx,dy) = 1 + 6 = 7 ints
        assert_eq!(f.geometry.len(), 7);
    }

    #[test]
    fn corpus_multilinestring_encodes_to_linestring_geomtype() {
        let tile = encode_one(wkb_multilinestring(&[
            &[(-10.0, 0.0), (10.0, 0.0)],
            &[(-5.0, 5.0), (5.0, 5.0), (5.0, -5.0)],
        ]));
        let f = &tile.layers[0].features[0];
        assert_eq!(f.r#type, Some(mvt::proto::GeomType::Linestring as i32));
        assert!(!f.geometry.is_empty());
    }

    #[test]
    fn corpus_multipolygon_encodes_to_polygon_geomtype_with_two_close_paths() {
        let ring_a: &[(f64, f64)] = &[
            (-30.0, -30.0),
            (-10.0, -30.0),
            (-10.0, -10.0),
            (-30.0, -10.0),
            (-30.0, -30.0),
        ];
        let ring_b: &[(f64, f64)] = &[
            (10.0, 10.0),
            (30.0, 10.0),
            (30.0, 30.0),
            (10.0, 30.0),
            (10.0, 10.0),
        ];
        let polys: &[&[&[(f64, f64)]]] = &[&[ring_a], &[ring_b]];
        let tile = encode_one(wkb_multipolygon(polys));
        let f = &tile.layers[0].features[0];
        assert_eq!(f.r#type, Some(mvt::proto::GeomType::Polygon as i32));
        // ClosePath(1) command-int = 15. Should appear at least twice.
        let close_count = f.geometry.iter().filter(|&&c| c == 15).count();
        assert_eq!(close_count, 2);
    }

    #[test]
    fn wkb_bbox_returns_envelope_for_each_geom_type() {
        let cases: Vec<(Bytes, (f64, f64, f64, f64))> = vec![
            (wkb_point(3.0, 4.0), (3.0, 4.0, 3.0, 4.0)),
            (
                wkb_linestring(&[(1.0, 2.0), (3.0, 4.0), (-1.0, 0.0)]),
                (-1.0, 0.0, 3.0, 4.0),
            ),
            (
                wkb_polygon(&[&[
                    (0.0, 0.0),
                    (10.0, 0.0),
                    (10.0, 10.0),
                    (0.0, 10.0),
                    (0.0, 0.0),
                ]]),
                (0.0, 0.0, 10.0, 10.0),
            ),
            (
                wkb_multipoint(&[(0.0, 0.0), (5.0, -5.0)]),
                (0.0, -5.0, 5.0, 0.0),
            ),
        ];
        for (wkb, expected) in cases {
            let got = wkb_bbox(&wkb).expect("bbox computed");
            assert!(
                (got.0 - expected.0).abs() < 1e-9
                    && (got.1 - expected.1).abs() < 1e-9
                    && (got.2 - expected.2).abs() < 1e-9
                    && (got.3 - expected.3).abs() < 1e-9,
                "bbox mismatch: got {got:?} expected {expected:?}"
            );
        }
    }
}
mod wkb_mvt_parity {
    //! Extended-capability tests for the geozero-backed WKB→MVT path.
    //!
    //! These were originally cross-impl parity tests (legacy hand-rolled
    //! parser vs. geozero adapter). After the legacy parser was deleted in
    //! the geozero migration, the parity tests for the 6 base geometry
    //! types are subsumed by `wkb_mvt_corpus.rs`. This file now retains
    //! only the inputs the *old* path rejected — they verify the migration
    //! genuinely unlocked new capability.

    use bytes::Bytes;
    use maptile_core::wkb_mvt::{WkbFeature, encode_features_to_mvt, wkb_bbox};

    const ENV: (f64, f64, f64, f64) = (-180.0, -90.0, 180.0, 90.0);
    const EXTENT: u32 = 4096;

    fn encode(wkb: Bytes) -> maptile_core::sources::mvt::proto::Tile {
        let bytes = encode_features_to_mvt(
            "L",
            std::iter::once(WkbFeature {
                id: None,
                wkb,
                properties: serde_json::Map::new(),
            }),
            ENV,
            EXTENT,
        )
        .expect("encode succeeded");
        maptile_core::sources::mvt::decode(&bytes).expect("decode succeeded")
    }

    #[test]
    fn accepts_3d_point_dropping_z() {
        // 3D Point WKB (type code 1 + 1000 = 1001).
        let mut v = Vec::new();
        v.push(0x01);
        v.extend_from_slice(&1001u32.to_le_bytes());
        v.extend_from_slice(&10.0_f64.to_le_bytes());
        v.extend_from_slice(&20.0_f64.to_le_bytes());
        v.extend_from_slice(&999.0_f64.to_le_bytes());
        let tile = encode(Bytes::from(v));
        assert_eq!(tile.layers[0].features.len(), 1);
    }

    #[test]
    fn accepts_ewkb_with_srid_prefix() {
        // EWKB Point with SRID 4326. EWKB sets the high bit `0x20000000`
        // on the type code and prefixes the 4-byte SRID after the type.
        let mut v = Vec::new();
        v.push(0x01);
        let type_code: u32 = 1 | 0x20000000;
        v.extend_from_slice(&type_code.to_le_bytes());
        v.extend_from_slice(&4326u32.to_le_bytes());
        v.extend_from_slice(&10.0_f64.to_le_bytes());
        v.extend_from_slice(&20.0_f64.to_le_bytes());
        let tile = encode(Bytes::from(v));
        assert_eq!(tile.layers[0].features.len(), 1);
    }

    fn push_u32(bytes: &mut Vec<u8>, value: u32, little_endian: bool) {
        bytes.extend_from_slice(&if little_endian {
            value.to_le_bytes()
        } else {
            value.to_be_bytes()
        });
    }

    fn dimensional_geometry(kind: u32, dims: u32, little_endian: bool, ewkb: bool) -> Bytes {
        let mut bytes = vec![u8::from(little_endian)];
        let type_id = if ewkb {
            kind | 0x2000_0000
                | if matches!(dims, 1 | 3) {
                    0x8000_0000
                } else {
                    0
                }
                | if matches!(dims, 2 | 3) {
                    0x4000_0000
                } else {
                    0
                }
        } else {
            kind + dims * 1000
        };
        push_u32(&mut bytes, type_id, little_endian);
        if ewkb {
            push_u32(&mut bytes, 4326, little_endian);
        }
        if kind >= 4 {
            push_u32(&mut bytes, 1, little_endian);
            let child = if kind == 7 { 1 } else { kind - 3 };
            bytes.extend_from_slice(&dimensional_geometry(child, dims, !little_endian, ewkb));
        } else {
            let coordinates: &[(f64, f64)] = match kind {
                1 => &[(10.0, 20.0)],
                2 => &[(10.0, 20.0), (20.0, 30.0)],
                3 => &[(10.0, 20.0), (20.0, 20.0), (20.0, 30.0), (10.0, 20.0)],
                _ => unreachable!(),
            };
            if kind == 3 {
                push_u32(&mut bytes, 1, little_endian);
            }
            if kind != 1 {
                push_u32(&mut bytes, coordinates.len() as u32, little_endian);
            }
            for &(x, y) in coordinates {
                let ordinates = [x, y, 999.0, -333.0];
                let count = match dims {
                    0 => 2,
                    1 | 2 => 3,
                    3 => 4,
                    _ => unreachable!(),
                };
                for value in &ordinates[..count] {
                    bytes.extend_from_slice(&if little_endian {
                        value.to_le_bytes()
                    } else {
                        value.to_be_bytes()
                    });
                }
            }
        }
        Bytes::from(bytes)
    }

    #[test]
    fn iso_and_ewkb_dimensions_and_mixed_endianness_preserve_xy() {
        for kind in 1..=7 {
            let xy = dimensional_geometry(kind, 0, true, false);
            let expected_bbox = wkb_bbox(&xy);
            let expected = encode(xy);
            for dims in 0..=3 {
                for little_endian in [false, true] {
                    for ewkb in [false, true] {
                        let wkb = dimensional_geometry(kind, dims, little_endian, ewkb);
                        assert_eq!(wkb_bbox(&wkb), expected_bbox);
                        let tile = encode(wkb);
                        assert_eq!(tile.layers[0].features, expected.layers[0].features);
                    }
                }
            }
        }
    }

    #[test]
    fn every_truncation_of_dimensional_geometries_is_rejected() {
        for kind in 1..=7 {
            for dims in 0..=3 {
                for little_endian in [false, true] {
                    for ewkb in [false, true] {
                        let wkb = dimensional_geometry(kind, dims, little_endian, ewkb);
                        for len in 0..wkb.len() {
                            let truncated = wkb.slice(..len);
                            assert!(wkb_bbox(&truncated).is_none());
                            let result = encode_features_to_mvt(
                                "x",
                                [WkbFeature {
                                    id: None,
                                    wkb: truncated,
                                    properties: serde_json::Map::new(),
                                }],
                                ENV,
                                EXTENT,
                            );
                            assert!(result.is_err(), "kind={kind}, dims={dims}, len={len}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn rejects_oversized_blob() {
        let huge = Bytes::from(vec![0u8; 17 * 1024 * 1024]);
        let res = encode_features_to_mvt(
            "x",
            std::iter::once(WkbFeature {
                id: None,
                wkb: huge,
                properties: serde_json::Map::new(),
            }),
            ENV,
            EXTENT,
        );
        assert!(res.is_err());
    }
}
mod wkb_mvt_adversarial {
    //! Adversarial WKB inputs — must error cleanly without panic, OOM, or
    //! unbounded recursion. These tests assert the public contract of
    //! `encode_features_to_mvt`: any malformed/oversized input returns
    //! `Err(_)` rather than aborting the process.

    use bytes::Bytes;
    use maptile_core::error::MaptileError;
    use maptile_core::wkb_mvt::{WkbFeature, encode_features_to_mvt, wkb_bbox};

    const ENV: (f64, f64, f64, f64) = (-180.0, -90.0, 180.0, 90.0);
    const EXTENT: u32 = 4096;

    fn must_err(wkb: Bytes) {
        assert!(
            wkb_bbox(&wkb).is_none(),
            "malformed WKB must not have a bbox"
        );
        let res = encode_features_to_mvt(
            "x",
            std::iter::once(WkbFeature {
                id: None,
                wkb,
                properties: serde_json::Map::new(),
            }),
            ENV,
            EXTENT,
        );
        assert!(
            matches!(res, Err(MaptileError::Transcode(ref message)) if message.starts_with("wkb validation:")),
            "expected rejection before the allocating decoder, got {res:?}"
        );
    }

    #[test]
    fn random_16mb_blob_errors_without_oom() {
        // Deterministic pseudo-random fill — we never want a real RNG dep here.
        // 16 MB exercises the size-cap guard in the new path and would OOM the
        // old path's safe_capacity heuristic if N were unbounded.
        let mut v = Vec::with_capacity(16 * 1024 * 1024);
        let mut x: u32 = 0x12345678;
        while v.len() < 16 * 1024 * 1024 {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            v.extend_from_slice(&x.to_le_bytes());
        }
        must_err(Bytes::from(v));
    }

    #[test]
    fn polygon_header_with_u32_max_rings_errors() {
        // Polygon header (5 bytes) + claimed 0xFFFFFFFF rings, no further bytes.
        let mut v = vec![0x01u8];
        v.extend_from_slice(&3u32.to_le_bytes());
        v.extend_from_slice(&u32::MAX.to_le_bytes());
        must_err(Bytes::from(v));
    }

    #[test]
    fn truncated_4_byte_wkb_errors() {
        must_err(Bytes::from(vec![0x01u8, 0x01, 0x00, 0x00]));
    }

    #[test]
    fn multipoint_claiming_huge_count_errors() {
        // MultiPoint header claiming 0xFFFFFFFF children, no payload.
        let mut v = vec![0x01u8];
        v.extend_from_slice(&4u32.to_le_bytes());
        v.extend_from_slice(&u32::MAX.to_le_bytes());
        must_err(Bytes::from(v));
    }

    #[test]
    fn multipolygon_with_thousand_phantom_children_errors() {
        // A child count must fit the payload before any children are decoded.
        let mut v = vec![0x01u8];
        v.extend_from_slice(&6u32.to_le_bytes()); // MultiPolygon
        v.extend_from_slice(&1000u32.to_le_bytes()); // n children
        must_err(Bytes::from(v));
    }

    #[test]
    fn every_counted_geometry_rejects_u32_max_in_both_dialects_and_byte_orders() {
        for kind in [2u32, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 15, 16, 17] {
            for type_id in [
                kind,
                kind + 1000,
                kind + 2000,
                kind + 3000,
                kind | 0xe000_0000,
            ] {
                for little_endian in [false, true] {
                    let mut bytes = vec![u8::from(little_endian)];
                    bytes.extend_from_slice(&if little_endian {
                        type_id.to_le_bytes()
                    } else {
                        type_id.to_be_bytes()
                    });
                    if type_id & 0x2000_0000 != 0 {
                        bytes.extend_from_slice(&4326u32.to_le_bytes());
                    }
                    bytes.extend_from_slice(&u32::MAX.to_le_bytes());
                    must_err(Bytes::from(bytes));
                }
            }
        }
    }

    fn counted_geometry(kind: u32, count: u32, payload: &[u8]) -> Bytes {
        let mut bytes = vec![1];
        bytes.extend_from_slice(&kind.to_le_bytes());
        bytes.extend_from_slice(&count.to_le_bytes());
        bytes.extend_from_slice(payload);
        Bytes::from(bytes)
    }

    #[test]
    fn nested_counts_and_polygon_ring_counts_are_checked() {
        let huge_line = counted_geometry(2, u32::MAX, &[]);
        let huge_polygon = counted_geometry(3, 1, &u32::MAX.to_le_bytes());
        for child in [
            counted_geometry(5, 1, &huge_line),
            counted_geometry(6, 1, &huge_polygon),
            counted_geometry(4, u32::MAX, &[0; 21]),
        ] {
            must_err(counted_geometry(7, 1, &child));
        }
    }

    #[test]
    fn zero_length_rings_and_untagged_curves_error_without_capacity_underflow() {
        must_err(counted_geometry(3, 1, &0u32.to_le_bytes()));
        let empty_line = counted_geometry(2, 0, &[]);
        for kind in [9, 10, 11] {
            must_err(counted_geometry(kind, 1, &empty_line));
        }
    }

    #[test]
    fn mismatched_child_types_and_trailing_bytes_are_rejected() {
        let mut point = vec![1];
        point.extend_from_slice(&1u32.to_le_bytes());
        point.extend_from_slice(&[0; 16]);
        for kind in [5, 6, 9, 10, 11, 12, 15, 16] {
            must_err(counted_geometry(kind, 1, &point));
        }
        point.push(0);
        must_err(Bytes::from(point));
    }

    #[test]
    fn deeply_nested_collections_error_without_stack_overflow() {
        let collection = counted_geometry(7, 1, &[]);
        let mut bytes = Vec::new();
        for _ in 0..10_000 {
            bytes.extend_from_slice(&collection);
        }
        bytes.extend_from_slice(&counted_geometry(7, 0, &[]));
        must_err(Bytes::from(bytes));
    }
}
