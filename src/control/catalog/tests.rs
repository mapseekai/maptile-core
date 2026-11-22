use std::{
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

use tokio::{sync::oneshot, time::Instant};

use super::{DiscoveredDataset, Inventory, SourceCatalog};
use crate::{
    cache::{ContentVersion, TileCache},
    control::{CatalogService, SourceDescriptor, TileDriver, open_blocking},
    error::{MaptileError, MaptileResult},
    service::{ReadTarget, TileStack},
    sources::{TileCoord, TileEncoding, TileFormat, TileInfo, TileQuery, TileResponse},
};

type WorkSignals = (oneshot::Sender<()>, mpsc::Receiver<()>);
struct Blocked(Mutex<Option<WorkSignals>>);

#[tokio::test]
async fn shutdown_remembers_unconfirmed_discovery_work() {
    let catalog = SourceCatalog::default();
    let items = [DiscoveredDataset {
        id: "remote".into(),
        dataset: "places".into(),
        revision: ContentVersion::from_input(b"v1"),
    }];
    let result = catalog
        .reconcile(0, "db", Inventory::Complete(&items), |_| async {
            crate::transport::CATALOG_BUILD_LEASE.with(|lease| lease.mark_unconfirmed());
            Err(MaptileError::Cancelled)
        })
        .await;
    assert!(matches!(result, Err(MaptileError::Cancelled)));
    for _ in 0..2 {
        assert!(matches!(
            catalog
                .shutdown(Instant::now() + Duration::from_secs(1))
                .await,
            Err(MaptileError::SourceUnavailable(_))
        ));
    }
}

#[tokio::test]
async fn shutdown_waits_for_cancelled_factory_file_work() {
    let catalog = SourceCatalog::default();
    let items = [DiscoveredDataset {
        id: "local".into(),
        dataset: "archive".into(),
        revision: ContentVersion::from_input(b"v1"),
    }];
    let (started, ready) = oneshot::channel();
    let (release, wait) = mpsc::channel();
    let mut signals = Some((started, wait));
    let mut build = Box::pin(
        catalog.reconcile(0, "files", Inventory::Complete(&items), |_| {
            let (started, wait) = signals.take().unwrap();
            open_blocking(move || {
                started.send(()).unwrap();
                wait.recv_timeout(Duration::from_secs(5)).unwrap();
                Err(MaptileError::Cancelled)
            })
        }),
    );
    tokio::select! {
        result = build.as_mut() => panic!("factory finished early: {result:?}"),
        ready = ready => ready.unwrap(),
    }
    drop(build);
    let mut shutdown = Box::pin(catalog.shutdown(Instant::now() + Duration::from_secs(5)));
    let first = futures::poll!(shutdown.as_mut());
    release.send(()).unwrap();
    assert!(
        first.is_pending(),
        "shutdown completed during file work: {first:?}"
    );
    shutdown.await.unwrap();
}

#[async_trait::async_trait]
impl TileDriver for Blocked {
    fn descriptor(&self) -> SourceDescriptor {
        SourceDescriptor::new(
            "retired",
            TileInfo::new(TileFormat::Mvt, TileEncoding::Uncompressed),
            ContentVersion::from_input(b"v1"),
        )
    }
    async fn read(
        &self,
        _: TileCoord,
        _: Option<&TileQuery<'_>>,
    ) -> MaptileResult<Option<TileResponse>> {
        let (began, wait) = self.0.lock().unwrap().take().unwrap();
        crate::transport::run_blocking(move || {
            let _ = began.send(());
            let _ = wait.recv_timeout(Duration::from_secs(5));
            Ok(None)
        })
        .await
    }
}
#[tokio::test]
async fn shutdown_waits_for_cancelled_job_on_retired_source() {
    let (began, started) = oneshot::channel();
    let (release, wait) = mpsc::channel();
    let engine = Arc::new(CatalogService::new(TileCache::new(1, None, None).unwrap()));
    let mut update = engine.catalog().update();
    update
        .put(Arc::new(Blocked(Mutex::new(Some((began, wait))))))
        .unwrap();
    engine.catalog().publish(update).unwrap();
    let stack = TileStack::builder(engine.clone()).build();
    let mut read = Box::pin(
        stack.read(ReadTarget::new("retired", "visible").at(TileCoord { z: 0, x: 0, y: 0 })),
    );
    tokio::select! { r = read.as_mut() => panic!("driver returned early {r:?}"), started = started => started.unwrap(), }
    let mut remove = engine.catalog().update();
    remove.remove("retired");
    engine.catalog().publish(remove).unwrap();
    drop(read);
    let mut drain = Box::pin(
        engine
            .catalog()
            .shutdown(Instant::now() + Duration::from_secs(5)),
    );
    let first = futures::poll!(drain.as_mut());
    release.send(()).unwrap();
    assert!(
        first.is_pending(),
        "shutdown reported {first:?} while tracked file work was still blocked"
    );
    drain.await.unwrap();
}
