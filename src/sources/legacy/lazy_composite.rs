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

use std::{
    any::Any,
    collections::HashMap,
    sync::{Arc, Weak},
};

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
    // Read the same member instances whose style capabilities were checked.
    static STYLE_MEMBERS: PinnedStyleMembers;
}

type ResolvedComposite = Option<(Arc<SourceManager>, Arc<CompositeSource>)>;
type PinnedStyleMembers = HashMap<usize, (StyleMembers, bool)>;

enum StyleMembers {
    Fixed,
    Live(ResolvedComposite),
}

pub(super) fn needs_raster_style_members(
    source: &dyn TileSource,
    query: Option<&TileQuery<'_>>,
) -> bool {
    query.is_some_and(TileQuery::has_raster_style)
        && !STYLE_MEMBERS
            .try_with(|members| members.contains_key(&std::ptr::from_ref(source).addr()))
            .unwrap_or(false)
        && !source.supports_catalog_raster_style()
}

pub(super) async fn with_raster_style_members<T>(
    source: &dyn TileSource,
    read: impl Future<Output = MaptileResult<T>>,
) -> MaptileResult<T> {
    let mut members = HashMap::new();
    if !pin_style_members(source, &mut members, &mut Vec::new())? {
        return Err(unsupported_raster_style(source.id()));
    }
    STYLE_MEMBERS.scope(members, read).await
}

fn pin_style_members(
    source: &dyn TileSource,
    members: &mut PinnedStyleMembers,
    path: &mut Vec<usize>,
) -> MaptileResult<bool> {
    let identity = std::ptr::from_ref(source).addr();
    if let Some((_, has_leaf)) = members.get(&identity) {
        return Ok(*has_leaf);
    }
    let any = source as &dyn Any;
    let resolved = if let Some(source) = any.downcast_ref::<LazyCompositeSource>() {
        source.push_path(path)?;
        let snapshot = snapshot(&source.id, &source.member_ids, &source.manager)?;
        let has_leaf = if let Some((_, composite)) = &snapshot {
            pin_style_members(composite.as_ref(), members, path)?
        } else {
            false
        };
        path.pop();
        (StyleMembers::Live(snapshot), has_leaf)
    } else if let Some(composite) = any.downcast_ref::<CompositeSource>() {
        let mut has_leaf = false;
        for child in composite.children() {
            has_leaf |= pin_style_members(child.source.as_ref(), members, path)?;
        }
        (StyleMembers::Fixed, has_leaf)
    } else if source.supports_raster_style() {
        return Ok(true);
    } else {
        return Err(unsupported_raster_style(source.id()));
    };
    let has_leaf = resolved.1;
    members.insert(identity, resolved);
    Ok(has_leaf)
}

fn unsupported_raster_style(id: &str) -> MaptileError {
    MaptileError::Unsupported(format!("{id} cannot apply raster style"))
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

    fn push_path(&self, path: &mut Vec<usize>) -> MaptileResult<()> {
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
        Ok(())
    }

    async fn read_in_scope<T>(
        &self,
        query: Option<&TileQuery<'_>>,
        read: impl Future<Output = MaptileResult<T>>,
    ) -> MaptileResult<T> {
        let mut path = RESOLUTION_PATH.try_with(Clone::clone).unwrap_or_default();
        self.push_path(&mut path)?;
        if needs_raster_style_members(self, query) {
            return RESOLUTION_PATH
                .scope(path, Box::pin(with_raster_style_members(self, read)))
                .await;
        }
        RESOLUTION_PATH.scope(path, read).await
    }

    fn read_snapshot(&self, query: Option<&TileQuery<'_>>) -> MaptileResult<ResolvedComposite> {
        if query.is_some_and(TileQuery::has_raster_style)
            && let Ok(Some(pinned)) = STYLE_MEMBERS.try_with(|members| {
                match members.get(&std::ptr::from_ref(self).addr()) {
                    Some((StyleMembers::Live(snapshot), _)) => Some(snapshot.clone()),
                    _ => None,
                }
            })
        {
            return Ok(pinned);
        }
        snapshot(&self.id, &self.member_ids, &self.manager)
    }
}

/// Resolve present members and build a short-lived [`CompositeSource`].
/// `None` means no members are present; loading, configuration and lifecycle
/// failures remain errors.
fn snapshot(
    id: &str,
    member_ids: &[String],
    manager: &Weak<SourceManager>,
) -> MaptileResult<ResolvedComposite> {
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
    CompositeSource::new(id, children).map(|composite| Some((mgr, Arc::new(composite))))
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
    fn supports_raster_style(&self) -> bool {
        true
    }
    fn supports_catalog_raster_style(&self) -> bool {
        false
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
        crate::sources::validate_tile_request(&[xyz], query)?;
        self.read_in_scope(query, async {
            let (manager, composite) = self
                .read_snapshot(query)?
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
        crate::sources::validate_tile_request(coords, query)?;
        if coords.is_empty() && !query.is_some_and(TileQuery::has_raster_style) {
            return Ok(Vec::new());
        }
        self.read_in_scope(query, async {
            let Some((_manager, composite)) = self.read_snapshot(query)? else {
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
        crate::sources::validate_tile_request(&[xyz], query)?;
        self.read_in_scope(query, async {
            let (manager, composite) = self
                .read_snapshot(query)?
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
