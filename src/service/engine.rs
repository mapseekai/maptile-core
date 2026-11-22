use futures_util::FutureExt;

use super::ValidatedTileRequest;
use crate::{
    cache::{
        ContentVersion, Flights, StyleHash, TileCache, TileCacheKey, hashing::canonical_render_hash,
    },
    control::{CompositeLimits, SourceCatalog, SourceDescriptor},
    core::response::TileOutcome,
    error::{MaptileError, MaptileResult},
    sources::{MosaicPixelSelection, TileQuery, effective_pixel_selection},
};

pub struct CatalogService {
    flights: Flights,
    catalog: SourceCatalog,
    cache: TileCache,
}

impl CatalogService {
    pub fn new(cache: TileCache) -> Self {
        Self {
            catalog: SourceCatalog::default(),
            cache,
            flights: Default::default(),
        }
    }
    pub fn with_limits(cache: TileCache, limits: CompositeLimits) -> MaptileResult<Self> {
        if limits.max_concurrency == 0
            || limits.max_concurrency > tokio::sync::Semaphore::MAX_PERMITS
            || limits.max_leaves == 0
            || limits.max_depth == 0
            || limits.max_depth > 64
            || limits.max_decoded_bytes == 0
            || limits.max_compressed_bytes == 0
            || limits.max_work_bytes == 0
        {
            return Err(MaptileError::Config("invalid composite limits".into()));
        }
        Ok(Self {
            catalog: SourceCatalog::new(limits),
            cache,
            flights: Default::default(),
        })
    }
    pub fn catalog(&self) -> &SourceCatalog {
        &self.catalog
    }
    pub(super) fn limits(&self) -> CompositeLimits {
        self.catalog.limits()
    }

    pub(super) async fn read_canonical(
        &self,
        pinned: &ValidatedTileRequest<'_>,
    ) -> MaptileResult<TileOutcome> {
        let request = pinned.request;
        let query = request.target.query;
        let descriptor = pinned.descriptor();
        if !descriptor.cache_zoom.contains(request.coord.z)
            || query.is_some_and(TileQuery::bypasses_cache)
        {
            let result = pinned
                .entry
                .read(request.coord, query, self.limits())
                .await?;
            check_output(&result).await?;
            return Ok(result);
        }
        let style = query.and_then(|q| q.style);
        let (selection, style_hash) = render_identity(descriptor, query);
        let mut identity = blake3::Hasher::new();
        identity.update(&crate::cache::hashing::CACHE_SCHEMA_VERSION.to_le_bytes());
        identity.update(descriptor.revision.as_bytes());
        identity.update(request.target.scope.as_bytes());
        let coord = request.coord;
        let key = TileCacheKey::new(
            descriptor.id.clone(),
            ContentVersion::from_bytes(*identity.finalize().as_bytes()),
            coord.z,
            coord.x,
            coord.y,
            style_hash,
        );
        if let Some(tile) = self.cache.get_validated(&key).await? {
            return Ok(TileOutcome::Present(tile));
        }

        let has_query = query.is_some();
        let requested_hash = query.and_then(|q| q.style_hash);
        self.flights
            .get_or_fill(key.clone(), pinned.generation(), || {
                let cache = self.cache.clone();
                let entry = pinned.entry.clone();
                let limits = self.limits();
                let style = style.cloned();
                async move {
                    if let Some(tile) = cache.get_validated(&key).await? {
                        return Ok(TileOutcome::Present(tile));
                    }
                    let query = TileQuery {
                        style: style.as_ref(),
                        style_hash: requested_hash,
                        pixel_selection: selection,
                        ..Default::default()
                    };
                    let result = entry
                        .read(
                            coord,
                            (has_query || selection.is_some()).then_some(&query),
                            limits,
                        )
                        .await?;
                    check_output(&result).await?;
                    if let TileOutcome::Present(tile) = &result {
                        cache.insert_validated(key, tile.clone()).await;
                    }
                    Ok(result)
                }
                .boxed()
            })
            .await
    }
}

pub(super) fn render_identity(
    descriptor: &SourceDescriptor,
    query: Option<&TileQuery<'_>>,
) -> (Option<MosaicPixelSelection>, StyleHash) {
    let style = query.and_then(|q| q.style);
    let selection = descriptor
        .capabilities
        .pixel_selection
        .map(|s| effective_pixel_selection(query, s));
    let (size, format) = style.map_or((256, 0), |s| {
        use crate::raster::style::RasterOutputFormat::*;
        (
            s.tile_size,
            match s.format {
                Png => 0,
                Webp => 1,
                Jpeg => 2,
            },
        )
    });
    let render = canonical_render_hash(style, selection, size, 1, format);
    let style_hash = match query.and_then(|q| q.style_hash) {
        None => render,
        Some(hash) => {
            let mut h = blake3::Hasher::new();
            h.update(hash.as_bytes());
            h.update(render.as_bytes());
            StyleHash::from_bytes(*h.finalize().as_bytes())
        }
    };
    (selection, style_hash)
}

pub(super) async fn check_output(outcome: &TileOutcome) -> MaptileResult<()> {
    let Some(tile) = outcome.tile() else {
        return Ok(());
    };
    crate::transport::validate_tile_payload(tile).await
}
