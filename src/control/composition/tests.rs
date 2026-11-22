use std::{collections::HashMap, sync::Arc};

use bytes::Bytes;

use super::{
    CompiledComposite, CompositeDeclaration, CompositeLimits, DuplicateLayer, MemberFailurePolicy,
    SourceSlot, compile_graph,
};
use crate::{
    cache::{CacheZoomRange, ContentVersion},
    control::{SourceCapabilities, SourceDescriptor, TileDriver},
    core::response::TileOutcome,
    error::{MaptileError, MaptileResult},
    sources::{
        SourceKind, TileCoord, TileEncoding, TileFormat, TileInfo, TileQuery, TileResponse,
        mvt::{self, proto},
    },
};

use geozero::mvt::Message;
use tilejson::{TileJSON, VectorLayer};
use tokio::sync::Barrier;

#[derive(Clone)]
struct Fake {
    id: String,
    tj: TileJSON,
    data: Result<Bytes, MaptileError>,
    encoding: TileEncoding,
    read_barrier: Option<Arc<Barrier>>,
}
#[async_trait::async_trait]
impl TileDriver for Fake {
    fn descriptor(&self) -> SourceDescriptor {
        SourceDescriptor {
            id: Arc::from(self.id.as_str()),
            kind: SourceKind::Other,
            tilejson: self.tj.clone(),
            info: TileInfo {
                format: TileFormat::Mvt,
                encoding: self.encoding,
            },
            revision: ContentVersion::from_input(self.id.as_bytes()),
            cache_zoom: CacheZoomRange::default(),
            capabilities: SourceCapabilities {
                strict_mvt: true,
                ..Default::default()
            },
        }
    }
    async fn read(
        &self,
        _: TileCoord,
        _: Option<&TileQuery<'_>>,
    ) -> MaptileResult<Option<TileResponse>> {
        if let Some(barrier) = &self.read_barrier {
            barrier.wait().await;
        }
        match &self.data {
            Ok(data) => Ok(Some(TileResponse {
                data: data.clone(),
                info: self.descriptor().info,
                last_modified: None,
            })),
            Err(MaptileError::TileNotFound(..)) => Ok(None),
            Err(error) => Err(error.clone()),
        }
    }
}
fn tile(layers: &[(&str, u32)]) -> Bytes {
    let tile = proto::Tile {
        layers: layers
            .iter()
            .map(|(name, extent)| proto::Layer {
                version: 2,
                name: (*name).into(),
                features: vec![],
                keys: vec![],
                values: vec![],
                extent: Some(*extent),
            })
            .collect(),
    };
    let mut bytes = Vec::new();
    tile.encode(&mut bytes).unwrap();
    Bytes::from(bytes)
}
fn source(id: &str, declared: &[&str], data: Result<Bytes, MaptileError>) -> Arc<SourceSlot> {
    fixture_source(id, declared, data, TileEncoding::Uncompressed, None)
}
fn fixture_source(
    id: &str,
    declared: &[&str],
    data: Result<Bytes, MaptileError>,
    encoding: TileEncoding,
    read_barrier: Option<Arc<Barrier>>,
) -> Arc<SourceSlot> {
    let mut tj = tilejson::tilejson! { tiles: vec![] };
    tj.vector_layers = Some(
        declared
            .iter()
            .map(|name| VectorLayer::new((*name).into(), Default::default()))
            .collect(),
    );
    let fake = Fake {
        id: id.into(),
        tj,
        data,
        encoding,
        read_barrier,
    };
    Arc::new(SourceSlot::new(Arc::new(fake)).unwrap())
}
fn decl(
    id: &str,
    members: &[&str],
    duplicate_layer: DuplicateLayer,
    policy: MemberFailurePolicy,
    lazy: bool,
) -> CompositeDeclaration {
    CompositeDeclaration {
        id: Arc::from(id),
        members: members.iter().map(|s| Arc::from(*s)).collect(),
        lazy,
        duplicate_layer,
        failure_policy: policy,
    }
}
fn coord() -> TileCoord {
    TileCoord { z: 0, x: 0, y: 0 }
}
fn nodes(
    sources: Vec<Arc<SourceSlot>>,
    declarations: &[CompositeDeclaration],
) -> MaptileResult<HashMap<Arc<str>, Arc<CompiledComposite>>> {
    compile_graph(
        &sources
            .into_iter()
            .map(|s| (s.descriptor().id.clone(), s))
            .collect(),
        declarations,
        CompositeLimits::default(),
    )
}
async fn read(node: &CompiledComposite) -> MaptileResult<TileOutcome> {
    node.read(coord(), None, CompositeLimits::default()).await
}
fn data(outcome: TileOutcome) -> Bytes {
    match outcome {
        TileOutcome::Present(t) => t.data,
        _ => panic!("expected present"),
    }
}

#[test]
fn rejects_cycle_missing_regular_duplicate_leaf_and_unknown_layers() {
    let a = source("a", &["roads"], Ok(tile(&[("roads", 1)])));
    assert!(
        nodes(
            vec![],
            &[
                decl(
                    "x",
                    &["y"],
                    DuplicateLayer::Error,
                    MemberFailurePolicy::FailFast,
                    true
                ),
                decl(
                    "y",
                    &["x"],
                    DuplicateLayer::Error,
                    MemberFailurePolicy::FailFast,
                    true
                )
            ]
        )
        .is_err()
    );
    assert!(
        nodes(
            vec![],
            &[decl(
                "x",
                &["missing"],
                DuplicateLayer::Error,
                MemberFailurePolicy::FailFast,
                false
            )]
        )
        .is_err()
    );
    assert!(
        nodes(
            vec![a.clone()],
            &[decl(
                "x",
                &["a", "a"],
                DuplicateLayer::KeepFirst,
                MemberFailurePolicy::FailFast,
                false
            )]
        )
        .is_err()
    );
    assert!(
        nodes(
            vec![source("unknown", &[], Ok(Bytes::new()))],
            &[decl(
                "x",
                &["unknown"],
                DuplicateLayer::Error,
                MemberFailurePolicy::FailFast,
                false
            )]
        )
        .is_err()
    );
    let lazy = nodes(
        vec![],
        &[decl(
            "x",
            &["missing"],
            DuplicateLayer::Error,
            MemberFailurePolicy::FailFast,
            true,
        )],
    )
    .unwrap();
    assert!(lazy["x"].leaves().is_empty());
}
#[test]
fn depth_limit_counts_declarations_and_rejects_deep_graph_before_building() {
    let mut declarations = Vec::new();
    for n in 0..65 {
        declarations.push(CompositeDeclaration {
            id: format!("node{n}").into(),
            members: vec![if n == 64 {
                "leaf".into()
            } else {
                format!("node{}", n + 1).into()
            }],
            lazy: true,
            duplicate_layer: DuplicateLayer::Error,
            failure_policy: MemberFailurePolicy::FailFast,
        });
    }
    assert!(nodes(vec![], &declarations).is_err());
    declarations.remove(0);
    assert!(nodes(vec![], &declarations).is_ok());
}
#[tokio::test]
async fn conflicts_are_selected_from_actual_per_tile_layers() {
    let a = source("a", &["roads", "water"], Ok(tile(&[("roads", 11)])));
    let b = source(
        "b",
        &["roads", "water"],
        Ok(tile(&[("roads", 22), ("water", 33)])),
    );
    for (policy, extent) in [
        (DuplicateLayer::KeepFirst, 11),
        (DuplicateLayer::KeepLast, 22),
    ] {
        let graph = nodes(
            vec![a.clone(), b.clone()],
            &[decl(
                "x",
                &["a", "b"],
                policy,
                MemberFailurePolicy::FailFast,
                false,
            )],
        )
        .unwrap();
        let bytes = data(read(&graph["x"]).await.unwrap());
        let result = mvt::decode(&bytes).unwrap();
        assert_eq!(result.layers.len(), 2);
        assert_eq!(
            result
                .layers
                .iter()
                .find(|l| l.name == "roads")
                .unwrap()
                .extent,
            Some(extent)
        );
    }
}
#[tokio::test]
async fn rejects_undeclared_actual_layer_even_when_error_policy_declares_no_conflict() {
    let a = source("a", &["roads"], Ok(tile(&[("roads", 1)])));
    for encoding in [TileEncoding::Uncompressed, TileEncoding::Gzip] {
        let b = fixture_source(
            "b",
            &["water"],
            crate::transport::encode_as(&tile(&[("roads", 2)]), encoding),
            encoding,
            None,
        );
        let graph = nodes(
            vec![a.clone(), b],
            &[decl(
                "x",
                &["a", "b"],
                DuplicateLayer::Error,
                MemberFailurePolicy::FailFast,
                false,
            )],
        )
        .unwrap();
        assert!(matches!(
            read(&graph["x"]).await,
            Err(MaptileError::InvalidMetadata(_))
        ));
    }
    let duplicate = source(
        "duplicate",
        &["roads"],
        Ok(tile(&[("roads", 1), ("roads", 2)])),
    );
    let graph = nodes(
        vec![duplicate],
        &[decl(
            "x",
            &["duplicate"],
            DuplicateLayer::KeepFirst,
            MemberFailurePolicy::FailFast,
            false,
        )],
    )
    .unwrap();
    assert!(matches!(
        read(&graph["x"]).await,
        Err(MaptileError::InvalidMetadata(_))
    ));
}
#[tokio::test]
async fn empty_is_present_no_tile_is_absent_and_transient_skip_is_partial() {
    let empty = source("empty", &["roads"], Ok(Bytes::new()));
    let absent = source(
        "absent",
        &["water"],
        Err(MaptileError::TileNotFound(0, 0, 0)),
    );
    let unavailable = source(
        "fail",
        &["rivers"],
        Err(MaptileError::SourceUnavailable("fail".into())),
    );
    let graph = nodes(
        vec![empty, absent, unavailable],
        &[
            decl(
                "zero",
                &["empty", "absent"],
                DuplicateLayer::Error,
                MemberFailurePolicy::FailFast,
                false,
            ),
            decl(
                "partial",
                &["zero", "fail"],
                DuplicateLayer::Error,
                MemberFailurePolicy::SkipTransient,
                false,
            ),
        ],
    )
    .unwrap();
    assert!(data(read(&graph["zero"]).await.unwrap()).is_empty());
    match read(&graph["partial"]).await.unwrap() {
        TileOutcome::Partial {
            tile: Some(tile),
            failures,
        } => {
            assert!(tile.data.is_empty());
            assert_eq!(failures.len(), 1);
        }
        _ => panic!("expected partial"),
    }
}
#[tokio::test]
async fn nested_policy_and_total_limit_are_preserved() {
    let fail = source(
        "fail",
        &["failed"],
        Err(MaptileError::SourceUnavailable("fail".into())),
    );
    let ok = source("ok", &["ok"], Ok(tile(&[("ok", 1)])));
    let graph = nodes(
        vec![fail, ok],
        &[
            decl(
                "inner",
                &["fail"],
                DuplicateLayer::Error,
                MemberFailurePolicy::FailFast,
                false,
            ),
            decl(
                "outer",
                &["inner", "ok"],
                DuplicateLayer::Error,
                MemberFailurePolicy::SkipTransient,
                false,
            ),
        ],
    )
    .unwrap();
    assert!(matches!(
        read(&graph["outer"]).await.unwrap(),
        TileOutcome::Partial { tile: Some(_), .. }
    ));
    let limits = CompositeLimits {
        max_decoded_bytes: 1,
        ..CompositeLimits::default()
    };
    assert!(matches!(
        graph["outer"].read(coord(), None, limits).await,
        Err(MaptileError::PayloadTooLarge { .. })
    ));
}
#[tokio::test]
async fn all_skipped_is_an_error_and_invalid_input_is_never_skipped() {
    let a = source(
        "a",
        &["a"],
        Err(MaptileError::SourceUnavailable("a".into())),
    );
    let b = source(
        "b",
        &["b"],
        Err(MaptileError::SourceUnavailable("b".into())),
    );
    let graph = nodes(
        vec![a, b],
        &[decl(
            "x",
            &["a", "b"],
            DuplicateLayer::Error,
            MemberFailurePolicy::SkipTransient,
            false,
        )],
    )
    .unwrap();
    assert!(matches!(
        read(&graph["x"]).await,
        Err(MaptileError::SourceUnavailable(_))
    ));
    let invalid = source(
        "invalid",
        &["invalid"],
        Err(MaptileError::InvalidRequest("invalid".into())),
    );
    let graph = nodes(
        vec![invalid],
        &[decl(
            "x",
            &["invalid"],
            DuplicateLayer::Error,
            MemberFailurePolicy::SkipTransient,
            false,
        )],
    )
    .unwrap();
    assert!(matches!(
        read(&graph["x"]).await,
        Err(MaptileError::InvalidRequest(_))
    ));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compressed_members_respect_work_and_decoded_limits() {
    use std::io::Write;

    fn roads(name: &str, offset: u32) -> Bytes {
        let features = (0..3000)
            .map(|index| {
                let x = (131 * index + offset) % 3500;
                let y = (253 * index + offset) % 3500;
                proto::Feature {
                    id: Some(u64::from(index) + 1),
                    tags: vec![0, index % 2],
                    r#type: Some(proto::GeomType::Linestring as i32),
                    geometry: vec![9, 2 * x, 2 * y, 26, 6, 8, 14, 10, 4, 6],
                }
            })
            .collect();
        mvt::encode(&proto::Tile {
            layers: vec![proto::Layer {
                version: 2,
                name: name.into(),
                features,
                keys: vec!["class".into()],
                values: ["primary", "local"]
                    .map(|class| proto::Value {
                        string_value: Some(class.into()),
                        ..Default::default()
                    })
                    .into(),
                extent: Some(4096),
            }],
        })
    }

    let raw = [roads("streets", 0), roads("paths", 721)];
    let compressed = raw.each_ref().map(|raw| {
        let mut encoder = flate2::GzBuilder::new()
            .extra(vec![0; 65_000])
            .write(Vec::new(), flate2::Compression::default());
        encoder.write_all(raw).unwrap();
        Bytes::from(encoder.finish().unwrap())
    });
    let decoded_bytes = raw[0].len() + raw[1].len();
    let encoded_bytes = compressed[0].len() + compressed[1].len();
    let work_bytes = 2 * decoded_bytes;
    assert!(encoded_bytes > decoded_bytes);
    assert!(encoded_bytes + raw[0].len().max(raw[1].len()) < work_bytes);

    let barrier = Arc::new(Barrier::new(2));
    let graph = nodes(
        vec![
            fixture_source(
                "a",
                &["streets"],
                Ok(compressed[0].clone()),
                TileEncoding::Gzip,
                Some(barrier.clone()),
            ),
            fixture_source(
                "b",
                &["paths"],
                Ok(compressed[1].clone()),
                TileEncoding::Gzip,
                Some(barrier),
            ),
        ],
        &[decl(
            "x",
            &["a", "b"],
            DuplicateLayer::Error,
            MemberFailurePolicy::FailFast,
            false,
        )],
    )
    .unwrap();
    let limits = CompositeLimits {
        max_work_bytes: work_bytes,
        max_decoded_bytes: decoded_bytes,
        ..CompositeLimits::default()
    };
    let expected = [raw[0].as_ref(), raw[1].as_ref()].concat();
    let mut rejected = 0;
    for x in 0..32 {
        let coord = TileCoord { z: 6, x, y: 0 };
        match graph["x"].read(coord, None, limits).await {
            Ok(outcome) => assert_eq!(data(outcome), expected),
            Err(MaptileError::PayloadTooLarge { .. }) => rejected += 1,
            Err(error) => panic!("unexpected error for {coord:?}: {error}"),
        }
    }
    assert_eq!(rejected, 0, "valid tiles rejected at the exact work limit");

    for limits in [
        CompositeLimits {
            max_work_bytes: work_bytes - 1,
            ..limits
        },
        CompositeLimits {
            max_decoded_bytes: decoded_bytes - 1,
            ..limits
        },
    ] {
        assert!(matches!(
            graph["x"].read(coord(), None, limits).await,
            Err(MaptileError::PayloadTooLarge { .. })
        ));
    }
}

#[tokio::test]
async fn cumulative_decoded_budget_rejects_two_large_valid_members() {
    fn large(name: &str) -> Bytes {
        let tile = proto::Tile {
            layers: vec![proto::Layer {
                version: 2,
                name: name.into(),
                features: vec![],
                keys: vec!["a".repeat(9 * 1024 * 1024)],
                values: vec![],
                extent: Some(4096),
            }],
        };
        let mut bytes = Vec::new();
        tile.encode(&mut bytes).unwrap();
        Bytes::from(bytes)
    }
    let a = source("a", &["a"], Ok(large("a")));
    let b = source("b", &["b"], Ok(large("b")));
    let graph = nodes(
        vec![a, b],
        &[decl(
            "x",
            &["a", "b"],
            DuplicateLayer::Error,
            MemberFailurePolicy::FailFast,
            false,
        )],
    )
    .unwrap();
    let limits = CompositeLimits {
        max_compressed_bytes: 32 * 1024 * 1024,
        ..CompositeLimits::default()
    };
    assert!(matches!(
        graph["x"].read(coord(), None, limits).await,
        Err(MaptileError::PayloadTooLarge { .. })
    ));
}
