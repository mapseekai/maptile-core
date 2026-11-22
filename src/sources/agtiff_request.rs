use std::{
    future::Future,
    sync::{Arc, OnceLock},
    time::Duration,
};

use async_geotiff::{
    error::{AgError, AgResult},
    request::{DEFAULT_MAX_IN_FLIGHT_REQUESTS, RequestLimiter},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::control::SourceReadLease;
use crate::error::{MaptileError, MaptileResult};
use crate::transport::SOURCE_READ_LEASE;

static REQUESTS: OnceLock<Arc<Semaphore>> = OnceLock::new();

tokio::task_local! {
    pub(super) static ACTIVE_RASTER_REQUEST: Arc<ActiveRasterRequest>;
}

pub(super) struct ActiveRasterRequest {
    _permit: OwnedSemaphorePermit,
    _lease: Option<Arc<SourceReadLease>>,
}

fn admit() -> MaptileResult<OwnedSemaphorePermit> {
    REQUESTS
        .get_or_init(|| Arc::new(Semaphore::new(DEFAULT_MAX_IN_FLIGHT_REQUESTS)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| MaptileError::CapacityExhausted)
}

pub(super) async fn render<T, F>(work: impl FnOnce(RequestLimiter) -> F) -> MaptileResult<T>
where
    F: Future<Output = AgResult<T>>,
{
    let request = NativeRequest {
        limiter: RequestLimiter::new(1),
        active: Arc::new(ActiveRasterRequest {
            _permit: admit()?,
            _lease: SOURCE_READ_LEASE.try_with(Arc::clone).ok(),
        }),
    };
    ACTIVE_RASTER_REQUEST
        .scope(request.active.clone(), work(request.limiter.clone()))
        .await
        .map_err(crate::raster::agtiff_style::render_error)
}

pub(super) async fn reader_call<T: Send + 'static>(
    work: impl Future<Output = MaptileResult<T>> + Send + 'static,
) -> MaptileResult<T> {
    let Ok(lease) = SOURCE_READ_LEASE.try_with(Arc::clone) else {
        return work.await;
    };
    let active = Arc::new(ActiveRasterRequest {
        _permit: admit()?,
        _lease: Some(lease.clone()),
    });
    // Raw CogReader calls do not have request cancellation or an admission token.
    tokio::spawn(SOURCE_READ_LEASE.scope(lease, ACTIVE_RASTER_REQUEST.scope(active, work)))
        .await
        .map_err(MaptileError::backend)?
}

pub(super) fn mark_reader_error(error: AgError) -> AgError {
    if matches!(error, AgError::Tiff(_) | AgError::ResourceLimit(_)) {
        // A failed raw window can abort sibling decode futures without joining their work.
        let _ = SOURCE_READ_LEASE.try_with(|lease| lease.mark_unconfirmed());
    }
    error
}

struct NativeRequest {
    limiter: RequestLimiter,
    active: Arc<ActiveRasterRequest>,
}

impl Drop for NativeRequest {
    fn drop(&mut self) {
        if self.limiter.available_permits() != 0 {
            return;
        }
        // shortcut: async-geotiff 0.13 exposes no completion notification, replace the poll when it does.
        let limiter = self.limiter.clone();
        let active = self.active.clone();
        tokio::spawn(async move {
            let _active = active;
            while limiter.available_permits() == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        cache::{CacheZoomRange, TileCache},
        control::CatalogService,
        raster::style::{EffectiveStyle, RasterStyle},
        service::{ReadTarget, TileStack},
        sources::{AsyncCogSource, AsyncCogSourceConfig, CogObjectSource, TileCoord, TileQuery},
    };
    use bytes::Bytes;
    use object_store::{ObjectStoreExt, memory::InMemory, path::Path};

    #[test]
    fn revoked_cog_read_holds_the_lease_until_native_work_stops() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let store = Arc::new(InMemory::new());
            let key = Path::from("tile.tif");
            let data = Bytes::from_static(include_bytes!("../../tests/fixtures/cog_uint8_rgb.tif"));
            store.put(&key, data.into()).await.unwrap();
            let source = AsyncCogSource::open(AsyncCogSourceConfig {
                id: "native-cog".into(),
                source: CogObjectSource::ObjectStore { store, key: key.to_string() },
                name: None,
                description: None,
                attribution: None,
                minzoom: None,
                maxzoom: None,
                bounds: None,
                cache_zoom: Some(CacheZoomRange::disabled()),
                content_id: "native-drain-test".into(),
                block_cache: None,
            }).await.unwrap();
            let cpu = source.cpu_limiter();
            let capacity = cpu.available_permits();
            let engine = Arc::new(CatalogService::new(TileCache::new(1, None, None).unwrap()));
            let mut update = engine.catalog().update();
            update.put(Arc::new(source)).unwrap();
            engine.catalog().publish(update).unwrap();
            let stack = TileStack::builder(engine.clone()).build();
            let style = EffectiveStyle::from_raster_style(RasterStyle::from_value(serde_json::json!({
                "extensions": [raster_style::TILE_SCHEMA_URL],
                "version": "3.0",
                "renderer": {"type": "gray", "bidx": [1]},
                "raster:tile": {"format": "png", "tilesize": 256}
            })).unwrap()).unwrap();
            let query = TileQuery { style: Some(&style), ..Default::default() };
            let target = ReadTarget {
                query: Some(&query),
                ..ReadTarget::new("native-cog", "public")
            };

            let (entered, started) = tokio::sync::oneshot::channel();
            let (release, wait) = std::sync::mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                entered.send(()).unwrap();
                wait.recv_timeout(Duration::from_secs(10)).unwrap();
            });
            started.await.unwrap();
            let mut read = Box::pin(stack.read(target.at(TileCoord { z: 0, x: 0, y: 0 })));
            let native_job_submitted = async {
                while cpu.available_permits() == capacity {
                    tokio::task::yield_now().await;
                }
            };
            tokio::select! {
                result = read.as_mut() => panic!("read finished before submitting native work: {result:?}"),
                result = tokio::time::timeout(Duration::from_secs(5), native_job_submitted) => result.unwrap(),
            }
            let snapshot = engine.catalog().snapshot();
            let mut revoke = Box::pin(engine.catalog().revoke(
                &snapshot, "native-cog", tokio::time::Instant::now() + Duration::from_secs(5),
            ));
            assert!(futures_util::poll!(revoke.as_mut()).is_pending());
            assert!(matches!(read.await, Err(MaptileError::Cancelled)));
            let polled = futures_util::poll!(revoke.as_mut());
            release.send(()).unwrap();
            assert!(polled.is_pending(), "revocation returned {polled:?} while native work was queued");
            blocker.await.unwrap();
            revoke.await.unwrap();
        });
    }
}
