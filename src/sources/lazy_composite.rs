//! Lazily-resolved composite MVT source backing `vector_group` kinds.
//!
//! Holds member *source ids* instead of member sources: every `get_tile`
//! resolves the currently-registered members from the live [`SourceManager`]
//! and delegates to a short-lived [`CompositeSource`]. Members missing from
//! the registry are skipped (mosaic "any member available" semantics); a
//! member re-registered later is picked up on the next request with no group
//! re-registration.
//! Indirect cycles and paths deeper than 64 groups return `Config` on reads.
//! Sibling branches may share a group; their resolution paths remain independent.
//!
//! The advertised TileJSON is snapshotted once at `open` from the members
//! present at that moment. On the creation path members are READY (hence
//! registered) before the group exists, so the snapshot is complete; on cold
//! start the server's preload builds vector_group sources last.

use std::sync::{Arc, Weak};

use tilejson::{TileJSON, tilejson};

use crate::error::{MaptileError, MaptileResult};
use crate::sources::composite::{CompositeChild, CompositeSource};
use crate::sources::manager::SourceManager;
use crate::sources::{
    TileCoord, TileEncoding, TileEtagResponse, TileFormat, TileInfo, TileQuery, TileResponse,
    TileSource,
};

tokio::task_local! {
    // Each nested future scopes its own path, so sibling DAG branches do not conflict.
    static RESOLUTION_PATH: Vec<usize>;
}

pub struct LazyCompositeSource {
    id: String,
    member_ids: Vec<String>,
    manager: Weak<SourceManager>,
    tj: TileJSON,
}

impl LazyCompositeSource {
    #[must_use]
    pub fn open(id: String, member_ids: Vec<String>, manager: Weak<SourceManager>) -> Self {
        let tj = match snapshot(&id, &member_ids, &manager) {
            Ok(Some((_, c))) => c.tilejson().clone(),
            Ok(None) | Err(_) => tilejson! { tiles: vec![] },
        };
        Self {
            id,
            member_ids,
            manager,
            tj,
        }
    }

    async fn guard_cycle<T>(
        &self,
        read: impl std::future::Future<Output = MaptileResult<T>>,
    ) -> MaptileResult<T> {
        let mut path = RESOLUTION_PATH.try_with(Clone::clone).unwrap_or_default();
        // A static composite can retain an older instance with the same source id.
        let instance = std::ptr::from_ref(self).addr();
        if path.contains(&instance) {
            return Err(MaptileError::Config(format!(
                "composite source cycle at {}",
                self.id
            )));
        }
        if path.len() >= 64 {
            return Err(MaptileError::Config(
                "composite nesting exceeds 64 groups".into(),
            ));
        }
        path.push(instance);
        RESOLUTION_PATH.scope(path, read).await
    }
}

/// Resolve present members and build a short-lived [`CompositeSource`].
/// `None` means no members are present; loading, configuration and lifecycle
/// failures remain errors.
fn snapshot(
    id: &str,
    member_ids: &[String],
    manager: &Weak<SourceManager>,
) -> MaptileResult<Option<(Arc<SourceManager>, CompositeSource)>> {
    let mgr = manager
        .upgrade()
        .ok_or_else(|| MaptileError::Config("source manager dropped".into()))?;
    let mut children = Vec::new();
    let mut unavailable = false;
    for member in member_ids.iter().filter(|member| member.as_str() != id) {
        match mgr.require(member) {
            Ok(source) => children.push(CompositeChild::new(source)),
            Err(MaptileError::SourceNotFound(_)) => {}
            Err(MaptileError::SourceUnavailable(_)) => unavailable = true,
            Err(error) => return Err(error),
        }
    }
    if children.is_empty() {
        return if unavailable {
            Err(MaptileError::SourceUnavailable(id.into()))
        } else {
            Ok(None)
        };
    }
    CompositeSource::new(id, children).map(|composite| Some((mgr, composite)))
}

fn read_member<'a>(
    manager: &'a SourceManager,
    source: Arc<dyn TileSource>,
    xyz: TileCoord,
    query: Option<&'a TileQuery<'a>>,
) -> futures_util::future::BoxFuture<'a, MaptileResult<TileResponse>> {
    // Keep recursive live groups out of the larger cache future's polling stack.
    if !source.cache_zoom().contains(xyz.z) {
        Box::pin(async move { source.get_tile(xyz, query).await })
    } else {
        Box::pin(manager.get_tile_from_source(source, xyz, query))
    }
}

#[async_trait::async_trait]
impl TileSource for LazyCompositeSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn tilejson(&self) -> &TileJSON {
        &self.tj
    }

    fn tile_info(&self) -> TileInfo {
        TileInfo {
            format: TileFormat::Mvt,
            encoding: TileEncoding::Uncompressed,
        }
    }

    fn clone_source(&self) -> Box<dyn TileSource> {
        Box::new(Self {
            id: self.id.clone(),
            member_ids: self.member_ids.clone(),
            manager: self.manager.clone(),
            tj: self.tj.clone(),
        })
    }

    // The resolved composite validates every member in the pinned read.
    fn supports_filter(&self) -> bool {
        true
    }
    fn supports_bbox(&self) -> bool {
        true
    }
    fn supports_url_query(&self) -> bool {
        true
    }

    fn cache_zoom(&self) -> crate::cache::CacheZoomRange {
        // Member registrations may change between key construction, rendering,
        // and ETag calculation. Preserve one snapshot per request instead of
        // caching this live group; the manager caches its snapshotted members.
        crate::cache::CacheZoomRange::disabled()
    }

    async fn get_tile(
        &self,
        xyz: TileCoord,
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<TileResponse> {
        super::tile_math::validate_tile_request(&[xyz], query)?;
        self.guard_cycle(async {
            let (manager, composite) = snapshot(&self.id, &self.member_ids, &self.manager)?
                .ok_or(MaptileError::TileNotFound(xyz.z, xyz.x, xyz.y))?;
            composite
                .get_tile_with(xyz, query, |source, query| {
                    read_member(&manager, source, xyz, query)
                })
                .await
        })
        .await
    }

    async fn read_tile_batch(
        &self,
        coords: &[TileCoord],
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<Vec<TileResponse>> {
        super::tile_math::validate_tile_request(coords, query)?;
        if coords.is_empty() {
            return Ok(Vec::new());
        }
        self.guard_cycle(async {
            let Some((_manager, composite)) = snapshot(&self.id, &self.member_ids, &self.manager)?
            else {
                return Ok(coords
                    .iter()
                    .map(|_| TileResponse {
                        data: bytes::Bytes::new(),
                        info: self.tile_info(),
                        last_modified: None,
                    })
                    .collect());
            };
            composite.read_tile_batch(coords, query).await
        })
        .await
    }

    async fn get_tile_with_etag(
        &self,
        xyz: TileCoord,
        query: Option<&TileQuery<'_>>,
        if_none_match: Option<&str>,
    ) -> MaptileResult<TileEtagResponse> {
        super::tile_math::validate_tile_request(&[xyz], query)?;
        self.guard_cycle(async {
            let (manager, composite) = snapshot(&self.id, &self.member_ids, &self.manager)?
                .ok_or(MaptileError::TileNotFound(xyz.z, xyz.x, xyz.y))?;
            let resp = composite
                .get_tile_with(xyz, query, |source, query| {
                    read_member(&manager, source, xyz, query)
                })
                .await?;
            let etag = composite.tile_etag(&resp);
            if Some(etag.as_str()) == if_none_match {
                Ok(TileEtagResponse::NotModified)
            } else {
                Ok(TileEtagResponse::Fresh { resp, etag })
            }
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::mvt::{self, proto};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tilejson::{TileJSON, VectorLayer, tilejson};

    // --- minimal MVT fake (mirrors composite.rs's test fake) ---

    struct MvtFake {
        id: String,
        tj: TileJSON,
        bytes: bytes::Bytes,
        format: TileFormat,
        calls: Arc<AtomicUsize>,
    }

    impl MvtFake {
        fn new(id: &str, layer_names: &[&str]) -> Arc<Self> {
            let layers: Vec<proto::Layer> = layer_names
                .iter()
                .map(|n| proto::Layer {
                    version: 2,
                    name: (*n).into(),
                    features: vec![],
                    keys: vec![],
                    values: vec![],
                    extent: Some(4096),
                })
                .collect();
            let bytes = mvt::encode(&proto::Tile { layers });
            let mut tj = tilejson! { tiles: vec![] };
            tj.minzoom = Some(0);
            tj.maxzoom = Some(14);
            tj.vector_layers = Some(
                layer_names
                    .iter()
                    .map(|n| VectorLayer::new((*n).into(), Default::default()))
                    .collect(),
            );
            Arc::new(Self {
                id: id.into(),
                tj,
                bytes,
                format: TileFormat::Mvt,
                calls: Arc::new(AtomicUsize::new(0)),
            })
        }
    }

    #[async_trait::async_trait]
    impl TileSource for MvtFake {
        fn id(&self) -> &str {
            &self.id
        }
        fn content_version(&self) -> crate::cache::ContentVersion {
            crate::cache::ContentVersion::from_input(&self.bytes)
        }
        fn supports_filter(&self) -> bool {
            true
        }
        fn tilejson(&self) -> &TileJSON {
            &self.tj
        }
        fn tile_info(&self) -> TileInfo {
            TileInfo {
                format: self.format,
                encoding: TileEncoding::Uncompressed,
            }
        }
        fn clone_source(&self) -> Box<dyn TileSource> {
            Box::new(MvtFake {
                id: self.id.clone(),
                tj: self.tj.clone(),
                bytes: self.bytes.clone(),
                format: self.format,
                calls: self.calls.clone(),
            })
        }
        async fn get_tile(
            &self,
            _xyz: TileCoord,
            _query: Option<&TileQuery<'_>>,
        ) -> MaptileResult<TileResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            tokio::task::yield_now().await;
            Ok(TileResponse {
                data: self.bytes.clone(),
                info: self.tile_info(),
                last_modified: None,
            })
        }
    }

    // --- helpers ---

    fn manager_with(sources: Vec<Arc<dyn TileSource>>) -> Arc<SourceManager> {
        let mgr = Arc::new(SourceManager::new().with_cache(Arc::new(
            crate::cache::TileCache::new(1, None, None).unwrap(),
        )));
        for s in sources {
            mgr.register(s);
        }
        mgr
    }

    fn layer_names(resp: &TileResponse) -> Vec<String> {
        let raw = crate::transport::decode(&resp.data, resp.info.encoding).unwrap();
        let tile = mvt::decode(&raw).unwrap();
        tile.layers.iter().map(|l| l.name.clone()).collect()
    }

    // --- tests ---

    #[tokio::test]
    async fn indirect_cycles_fail_all_read_paths_and_recovery_is_possible() {
        let manager = manager_with(vec![]);
        let group = Arc::new(LazyCompositeSource::open(
            "a".into(),
            vec!["b".into()],
            Arc::downgrade(&manager),
        ));
        manager.register(group.clone());
        manager.register(Arc::new(LazyCompositeSource::open(
            "b".into(),
            vec!["a".into()],
            Arc::downgrade(&manager),
        )));
        let xyz = TileCoord { z: 0, x: 0, y: 0 };
        assert!(matches!(
            group.get_tile(xyz, None).await,
            Err(MaptileError::Config(_))
        ));
        assert!(matches!(
            group.read_tile_batch(&[xyz], None).await,
            Err(MaptileError::Config(_))
        ));
        assert!(matches!(
            group.get_tile_with_etag(xyz, None, None).await,
            Err(MaptileError::Config(_))
        ));
        manager.register(MvtFake::new("b", &["recovered"]));
        assert_eq!(
            layer_names(&group.get_tile(xyz, None).await.unwrap()),
            ["recovered"]
        );
    }

    #[tokio::test]
    async fn shared_nested_groups_are_valid_across_concurrent_branches() {
        let manager = manager_with(vec![MvtFake::new("leaf", &["roads"])]);
        for (id, members) in [
            ("shared", vec!["leaf"]),
            ("left", vec!["shared"]),
            ("right", vec!["shared"]),
            ("root", vec!["left", "right"]),
        ] {
            manager.register(Arc::new(LazyCompositeSource::open(
                id.into(),
                members.into_iter().map(str::to_owned).collect(),
                Arc::downgrade(&manager),
            )));
        }
        let root = manager.require("root").unwrap();
        let xyz = TileCoord { z: 0, x: 0, y: 0 };
        let (first, second) = tokio::join!(
            root.get_tile(xyz, None),
            root.get_tile_with_etag(xyz, None, None)
        );
        assert!(!first.unwrap().data.is_empty());
        assert!(second.is_ok());
        assert!(
            !root.read_tile_batch(&[xyz], None).await.unwrap()[0]
                .data
                .is_empty()
        );
    }

    #[tokio::test]
    async fn replacement_can_reach_an_older_instance_with_the_same_id() {
        let manager = manager_with(vec![MvtFake::new("leaf", &["roads"])]);
        manager.register(Arc::new(LazyCompositeSource::open(
            "a".into(),
            vec!["leaf".into()],
            Arc::downgrade(&manager),
        )));
        manager.register_composite_from_ids("b", &["a"]).unwrap();
        manager.register(Arc::new(LazyCompositeSource::open(
            "a".into(),
            vec!["b".into()],
            Arc::downgrade(&manager),
        )));
        let root = manager.require("a").unwrap();
        let xyz = TileCoord { z: 0, x: 0, y: 0 };
        assert_eq!(
            layer_names(&root.get_tile(xyz, None).await.unwrap()),
            ["roads"]
        );
        assert!(root.get_tile_with_etag(xyz, None, None).await.is_ok());
        assert_eq!(
            layer_names(&root.read_tile_batch(&[xyz], None).await.unwrap()[0]),
            ["roads"]
        );
        manager.register(Arc::new(LazyCompositeSource::open(
            "leaf".into(),
            vec!["a".into()],
            Arc::downgrade(&manager),
        )));
        assert!(matches!(
            root.get_tile(xyz, None).await,
            Err(MaptileError::Config(_))
        ));
    }

    #[tokio::test]
    async fn excessive_nesting_returns_an_error_before_stack_exhaustion() {
        let manager = manager_with(vec![MvtFake::new("leaf", &["roads"])]);
        let mut child = "leaf".to_owned();
        for n in 0..65 {
            let id = format!("group_{n}");
            manager.register(Arc::new(LazyCompositeSource::open(
                id.clone(),
                vec![child],
                Arc::downgrade(&manager),
            )));
            child = id;
        }
        let xyz = TileCoord { z: 0, x: 0, y: 0 };
        let at_limit = manager.require("group_63").unwrap();
        assert!(at_limit.get_tile(xyz, None).await.is_ok());
        assert!(at_limit.get_tile_with_etag(xyz, None, None).await.is_ok());
        assert!(at_limit.read_tile_batch(&[xyz], None).await.is_ok());
        assert!(matches!(
            manager.require(&child).unwrap().get_tile(xyz, None).await,
            Err(MaptileError::Config(_))
        ));
    }

    #[tokio::test]
    async fn merges_layers_of_present_members() {
        let a = MvtFake::new("vector.a", &["water"]);
        let b = MvtFake::new("vector.b", &["roads"]);
        let mgr = manager_with(vec![a, b]);

        let group = LazyCompositeSource::open(
            "group.g".into(),
            vec!["vector.a".into(), "vector.b".into()],
            Arc::downgrade(&mgr),
        );
        let resp = group
            .get_tile(TileCoord { z: 0, x: 0, y: 0 }, None)
            .await
            .unwrap();
        assert_eq!(layer_names(&resp), vec!["water", "roads"]);
    }

    #[tokio::test]
    async fn missing_member_is_skipped() {
        let a = MvtFake::new("vector.a", &["water"]);
        // vector.b is not registered
        let mgr = manager_with(vec![a]);

        let group = LazyCompositeSource::open(
            "group.g".into(),
            vec!["vector.a".into(), "vector.b".into()],
            Arc::downgrade(&mgr),
        );
        let resp = group
            .get_tile(TileCoord { z: 0, x: 0, y: 0 }, None)
            .await
            .unwrap();
        assert_eq!(layer_names(&resp), vec!["water"]);
    }

    #[tokio::test]
    async fn all_members_missing_is_tile_not_found() {
        let mgr = manager_with(vec![]);

        let group = LazyCompositeSource::open(
            "group.g".into(),
            vec!["vector.a".into(), "vector.b".into()],
            Arc::downgrade(&mgr),
        );
        let err = group
            .get_tile(TileCoord { z: 1, x: 0, y: 0 }, None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, MaptileError::TileNotFound(1, 0, 0)),
            "expected TileNotFound(1,0,0), got: {err:?}"
        );
    }

    #[tokio::test]
    async fn configuration_and_lifecycle_errors_are_not_missing_tiles() {
        for dropped_manager in [false, true] {
            let mut source = MvtFake::new("member", &["water"]);
            if !dropped_manager {
                Arc::get_mut(&mut source).unwrap().format = TileFormat::Png;
            }
            let manager = manager_with(vec![source]);
            let group = LazyCompositeSource::open(
                "group".into(),
                vec!["member".into()],
                Arc::downgrade(&manager),
            );
            if dropped_manager {
                drop(manager);
            }
            let xyz = TileCoord { z: 0, x: 0, y: 0 };
            let expected = if dropped_manager {
                "manager dropped"
            } else {
                "must be Mvt"
            };
            for error in [
                group.get_tile(xyz, None).await.unwrap_err(),
                group.get_tile_with_etag(xyz, None, None).await.unwrap_err(),
                group.read_tile_batch(&[xyz], None).await.unwrap_err(),
            ] {
                assert!(
                    matches!(error, MaptileError::Config(ref message) if message.contains(expected)),
                    "expected configuration/lifecycle error, got {error:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn manager_rejects_mosaic_selection_for_live_group_etag_reads() {
        let manager = manager_with(vec![MvtFake::new("member", &["water"])]);
        manager.register(Arc::new(LazyCompositeSource::open(
            "group".into(),
            vec!["member".into()],
            Arc::downgrade(&manager),
        )));
        let query = TileQuery {
            pixel_selection: Some(crate::sources::MosaicPixelSelection::Median),
            ..Default::default()
        };
        let xyz = TileCoord { z: 0, x: 0, y: 0 };
        assert!(matches!(
            manager.get_tile("group", xyz, Some(&query)).await,
            Err(MaptileError::Unsupported(_))
        ));
        assert!(matches!(
            manager
                .get_tile_with_etag("group", xyz, Some(&query), None)
                .await,
            Err(MaptileError::Unsupported(_))
        ));
    }

    #[tokio::test]
    async fn member_registered_after_open_is_picked_up() {
        // Open with an empty manager — snapshot is empty TileJSON.
        let mgr = Arc::new(SourceManager::new());
        let group = LazyCompositeSource::open(
            "group.g".into(),
            vec!["vector.a".into()],
            Arc::downgrade(&mgr),
        );

        // Before registration: TileNotFound.
        let err = group
            .get_tile(TileCoord { z: 0, x: 0, y: 0 }, None)
            .await
            .unwrap_err();
        assert!(matches!(err, MaptileError::TileNotFound(..)));

        // Register the member AFTER open — the lazy resolve picks it up.
        mgr.register(MvtFake::new("vector.a", &["water"]));

        let resp = group
            .get_tile(TileCoord { z: 0, x: 0, y: 0 }, None)
            .await
            .unwrap();
        assert_eq!(layer_names(&resp), vec!["water"]);
    }

    #[tokio::test]
    async fn unavailable_member_preserves_failure_until_recovered_or_deleted() {
        let mgr = Arc::new(SourceManager::new());
        let group =
            LazyCompositeSource::open("group".into(), vec!["member".into()], Arc::downgrade(&mgr));
        let xyz = TileCoord { z: 0, x: 0, y: 0 };
        mgr.mark_unavailable("member");
        assert!(matches!(
            group.get_tile(xyz, None).await,
            Err(MaptileError::SourceUnavailable(..))
        ));
        assert!(matches!(
            group.get_tile_with_etag(xyz, None, None).await,
            Err(MaptileError::SourceUnavailable(..))
        ));
        assert!(matches!(
            group.read_tile_batch(&[xyz], None).await,
            Err(MaptileError::SourceUnavailable(..))
        ));
        mgr.register(MvtFake::new("member", &["water"]));
        assert_eq!(
            layer_names(&group.get_tile(xyz, None).await.unwrap()),
            vec!["water"]
        );
        mgr.mark_unavailable("member");
        mgr.remove("member");
        assert!(matches!(
            group.get_tile(xyz, None).await,
            Err(MaptileError::TileNotFound(..))
        ));
    }

    #[tokio::test]
    async fn batch_keeps_missing_members_empty_and_observes_later_registration() {
        let mgr = Arc::new(SourceManager::new());
        let group =
            LazyCompositeSource::open("group".into(), vec!["member".into()], Arc::downgrade(&mgr));
        let coords = [TileCoord { z: 0, x: 0, y: 0 }; 2];
        let empty = group.read_tile_batch(&coords, None).await.unwrap();
        assert_eq!(empty.len(), 2);
        assert!(empty.iter().all(|tile| tile.data.is_empty()));
        mgr.register(MvtFake::new("member", &["water"]));
        let batch = group.read_tile_batch(&coords, None).await.unwrap();
        assert_eq!(batch.len(), 2);
        assert!(batch.iter().all(|tile| layer_names(tile) == ["water"]));
    }

    #[tokio::test]
    async fn live_members_share_manager_cache_but_queries_and_batches_bypass_it() {
        let member = MvtFake::new("member", &["roads"]);
        let mgr = manager_with(vec![member.clone()]);
        let group = Arc::new(LazyCompositeSource::open(
            "group".into(),
            vec!["member".into()],
            Arc::downgrade(&mgr),
        ));
        mgr.register(group.clone());
        let xyz = TileCoord { z: 0, x: 0, y: 0 };
        for _ in 0..2 {
            mgr.get_tile("member", xyz, None).await.unwrap();
            mgr.get_tile("group", xyz, None).await.unwrap();
        }
        mgr.get_tile("group", xyz, Some(&TileQuery::default()))
            .await
            .unwrap();
        let TileEtagResponse::Fresh { etag, .. } = mgr
            .get_tile_with_etag("group", xyz, None, None)
            .await
            .unwrap()
        else {
            panic!("expected fresh group response");
        };
        assert!(matches!(
            mgr.get_tile_with_etag("group", xyz, None, Some(&etag))
                .await
                .unwrap(),
            TileEtagResponse::NotModified
        ));
        assert_eq!(member.calls.load(Ordering::SeqCst), 1);

        let filter = crate::filter::parse_cql2("id=1").unwrap();
        let query = TileQuery {
            filter: Some(&filter),
            ..TileQuery::default()
        };
        for _ in 0..2 {
            mgr.get_tile("group", xyz, Some(&query)).await.unwrap();
        }
        assert_eq!(member.calls.load(Ordering::SeqCst), 3);
        group.read_tile_batch(&[xyz], None).await.unwrap();
        assert_eq!(member.calls.load(Ordering::SeqCst), 4);

        let cold = TileCoord { z: 1, x: 0, y: 0 };
        let (group, direct) = tokio::join!(
            mgr.get_tile("group", cold, None),
            mgr.get_tile("member", cold, None),
        );
        group.unwrap();
        direct.unwrap();
        assert_eq!(member.calls.load(Ordering::SeqCst), 5);
    }

    #[tokio::test]
    async fn cached_member_reads_keep_the_resolved_snapshot_after_replacement() {
        let old = MvtFake::new("member", &["water"]);
        let mgr = manager_with(vec![old.clone()]);
        let xyz = TileCoord { z: 0, x: 0, y: 0 };
        mgr.get_tile("member", xyz, None).await.unwrap();
        let (manager, composite) = snapshot("group", &["member".into()], &Arc::downgrade(&mgr))
            .unwrap()
            .unwrap();
        let new = MvtFake::new("member", &["roads"]);
        mgr.register(new.clone());
        let resp = composite
            .get_tile_with(xyz, None, |source, query| {
                manager.get_tile_from_source(source, xyz, query)
            })
            .await
            .unwrap();
        assert_eq!(layer_names(&resp), ["water"]);
        assert_eq!(old.calls.load(Ordering::SeqCst), 1);
        assert_eq!(new.calls.load(Ordering::SeqCst), 0);

        let group =
            LazyCompositeSource::open("group".into(), vec!["member".into()], Arc::downgrade(&mgr));
        assert_eq!(
            layer_names(&group.get_tile(xyz, None).await.unwrap()),
            ["roads"]
        );
        assert_eq!(new.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn manager_preserves_live_members_and_composite_etag_with_hot_cache() {
        let cache = Arc::new(crate::cache::TileCache::new(1, None, None).unwrap());
        let mgr = Arc::new(SourceManager::new().with_cache(cache));
        mgr.register(MvtFake::new("a", &["water"]));
        let group = Arc::new(LazyCompositeSource::open(
            "group".into(),
            vec!["a".into()],
            Arc::downgrade(&mgr),
        ));
        mgr.register(group.clone());
        let xyz = TileCoord { z: 0, x: 0, y: 0 };
        let (_, expected_source) = snapshot("group", &["a".into()], &Arc::downgrade(&mgr))
            .unwrap()
            .unwrap();
        let expected = expected_source
            .get_tile_with_etag(xyz, None, None)
            .await
            .unwrap();
        let expected_etag = match expected {
            TileEtagResponse::Fresh { etag, .. } => etag,
            _ => unreachable!(),
        };
        let actual = mgr
            .get_tile_with_etag("group", xyz, None, None)
            .await
            .unwrap();
        match actual {
            TileEtagResponse::Fresh { resp, etag } => {
                assert_eq!(layer_names(&resp), vec!["water"]);
                assert_eq!(etag, expected_etag);
            }
            _ => unreachable!(),
        }
        mgr.register(MvtFake::new("a", &["roads"]));
        let actual = mgr
            .get_tile_with_etag("group", xyz, None, Some(&expected_etag))
            .await
            .unwrap();
        match actual {
            TileEtagResponse::Fresh { resp, etag } => {
                assert_eq!(layer_names(&resp), vec!["roads"]);
                assert_ne!(etag, expected_etag);
            }
            _ => panic!("new live members must not return a cached validator"),
        }
    }

    #[tokio::test]
    async fn self_reference_is_skipped() {
        // A group listing its own id as a member must not recurse.
        let mgr = Arc::new(SourceManager::new());
        let group = Arc::new(LazyCompositeSource::open(
            "group.g".into(),
            vec!["group.g".into()],
            Arc::downgrade(&mgr),
        ));
        // Register the group itself in the manager (worst case).
        mgr.register(group.clone() as Arc<dyn TileSource>);

        // get_tile must yield TileNotFound (0 non-self members resolved),
        // not recurse or panic.
        let err = group
            .get_tile(TileCoord { z: 0, x: 0, y: 0 }, None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, MaptileError::TileNotFound(..)),
            "expected TileNotFound for self-reference, got: {err:?}"
        );
    }
}
