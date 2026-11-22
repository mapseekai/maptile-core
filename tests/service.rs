use async_trait::async_trait;
use bytes::Bytes;
use maptile_core::{
    cache::{ContentVersion, TileCache},
    control::*,
    error::{ErrorKind, MaptileError, MaptileResult},
    service::*,
    sources::{TileCoord, TileEncoding, TileFormat, TileInfo, TileQuery, TileResponse},
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::Notify;

const ORIGIN: TileCoord = TileCoord { z: 0, x: 0, y: 0 };
fn tile(bytes: Bytes) -> TileResponse {
    TileResponse {
        data: bytes,
        info: TileInfo::new(TileFormat::Mvt, TileEncoding::Uncompressed),
        last_modified: None,
    }
}

struct Driver {
    name: &'static str,
    revision: &'static str,
    bytes: Bytes,
    calls: AtomicUsize,
    hold: Option<Arc<Notify>>,
}
impl Driver {
    fn new(name: &'static str, revision: &'static str, data: Bytes) -> Self {
        Self {
            name,
            revision,
            bytes: data,
            calls: AtomicUsize::new(0),
            hold: None,
        }
    }
}
#[async_trait]
impl TileDriver for Driver {
    fn descriptor(&self) -> SourceDescriptor {
        let mut d = SourceDescriptor::new(
            self.name,
            tile(Bytes::new()).info,
            ContentVersion::from_input(self.revision.as_bytes()),
        );
        d.capabilities.consistent_export = true;
        d
    }
    async fn read(
        &self,
        coord: TileCoord,
        _: Option<&TileQuery<'_>>,
    ) -> MaptileResult<Option<TileResponse>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(hold) = &self.hold {
            hold.notified().await;
        }
        Ok((coord.y == 0).then(|| tile(self.bytes.clone())))
    }
}
fn engine(driver: Arc<Driver>) -> Arc<CatalogService> {
    let service = Arc::new(CatalogService::new(TileCache::new(32, None, None).unwrap()));
    let mut edit = service.catalog().update();
    edit.put(driver).unwrap();
    service.catalog().publish(edit).unwrap();
    service
}

#[derive(Clone)]
struct Record(&'static str, Arc<Mutex<Vec<String>>>);
#[async_trait]
impl TileLayer for Record {
    async fn read(
        &self,
        _: &ValidatedTileRequest<'_>,
        next: TileService<'_>,
    ) -> MaptileResult<TileOutcome> {
        self.1.lock().unwrap().push(format!("+{}", self.0));
        let result = next.read().await;
        self.1.lock().unwrap().push(format!("-{}", self.0));
        result
    }
}
struct Rewrite;
#[async_trait]
impl TileLayer for Rewrite {
    async fn read(
        &self,
        _: &ValidatedTileRequest<'_>,
        next: TileService<'_>,
    ) -> MaptileResult<TileOutcome> {
        let mut outcome = next.read().await?;
        if let TileOutcome::Present(tile) = &mut outcome {
            tile.data = Bytes::from_static(b"rewritten");
            tile.info = TileInfo::new(TileFormat::Json, TileEncoding::Uncompressed);
        }
        Ok(outcome)
    }
}
struct Short(MaptileResult<TileResponse>);
#[async_trait]
impl TileLayer for Short {
    async fn read(
        &self,
        _: &ValidatedTileRequest<'_>,
        _: TileService<'_>,
    ) -> MaptileResult<TileOutcome> {
        self.0.clone().map(TileOutcome::Present)
    }
}

#[tokio::test]
async fn layers_wrap_in_order_and_only_complete_canonical_bytes_enter_the_cache() {
    let driver = Arc::new(Driver::new("sample", "v1", Bytes::from_static(b"original")));
    let service = engine(driver.clone());
    let trace = Arc::new(Mutex::new(Vec::new()));
    let req = ReadTarget::new("sample", "tenant-a").at(ORIGIN);
    for names in [["a", "b"], ["b", "a"]] {
        let stack = TileStack::builder(service.clone())
            .layer(Record(names[0], trace.clone()))
            .layer(Record(names[1], trace.clone()))
            .layer(Rewrite)
            .build();
        assert_eq!(
            stack.read(req).await.unwrap().tile().unwrap().info.format,
            TileFormat::Json
        );
        assert_eq!(
            std::mem::take(&mut *trace.lock().unwrap()),
            [
                format!("+{}", names[0]),
                format!("+{}", names[1]),
                format!("-{}", names[1]),
                format!("-{}", names[0])
            ]
        );
    }
    let plain = TileStack::builder(service).build();
    assert_eq!(
        plain.read(req).await.unwrap().tile().unwrap().data,
        &b"original"[..]
    );
    assert_eq!(driver.calls.load(Ordering::SeqCst), 1);
    plain
        .read(ReadTarget::new("sample", "tenant-b").at(ORIGIN))
        .await
        .unwrap();
    assert_eq!(driver.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn admission_precedes_layers_and_shortcuts_obey_the_decoded_limit() {
    let driver = Arc::new(Driver::new("sample", "v1", Bytes::new()));
    let service = engine(driver.clone());
    let req = ReadTarget::new("sample", "public");
    let stack = TileStack::builder(service.clone())
        .layer(Short(Err(MaptileError::CapacityExhausted)))
        .build();
    assert_eq!(
        stack.read(req.at(ORIGIN)).await.unwrap_err().kind(),
        ErrorKind::Saturated
    );
    let bad = TileCoord { z: 0, x: 1, y: 0 };
    assert_eq!(
        stack.read(req.at(bad)).await.unwrap_err().kind(),
        ErrorKind::InvalidInput
    );
    let q = TileQuery {
        bbox: Some([0.0; 4]),
        ..Default::default()
    };
    assert_eq!(
        stack
            .read(
                ReadTarget {
                    query: Some(&q),
                    ..req
                }
                .at(ORIGIN)
            )
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::Unsupported
    );
    let data = vec![0; maptile_core::transport::MAX_DECODED_TILE_BYTES + 1];
    use std::io::Write;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(&data).unwrap();
    let mut gz = tile(Bytes::from(encoder.finish().unwrap()));
    gz.info.encoding = TileEncoding::Gzip;
    for invalid in [gz, tile(Bytes::from(data))] {
        let stack = TileStack::builder(service.clone())
            .layer(Short(Ok(invalid)))
            .build();
        assert_eq!(
            stack.read(req.at(ORIGIN)).await.unwrap_err().kind(),
            ErrorKind::ResourceLimit
        );
    }
    assert_eq!(driver.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn one_waiter_can_leave_a_shared_fill_and_the_remaining_waiter_gets_the_tile() {
    let hold = Arc::new(Notify::new());
    let driver = Arc::new(Driver {
        hold: Some(hold.clone()),
        ..Driver::new("sample", "v1", Bytes::new())
    });
    let stack = TileStack::builder(engine(driver.clone())).build();
    let req = ReadTarget::new("sample", "public").at(ORIGIN);
    let mut leader = Box::pin(stack.read(req));
    assert!(futures::poll!(leader.as_mut()).is_pending());
    let mut follower = Box::pin(stack.read(req));
    while driver.calls.load(Ordering::SeqCst) == 0 {
        tokio::select! { _=leader.as_mut()=>panic!("driver did not wait"), _=tokio::task::yield_now()=>{} }
    }
    assert!(futures::poll!(follower.as_mut()).is_pending());
    drop(leader);
    hold.notify_one();
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(2), follower)
            .await
            .unwrap()
            .unwrap(),
        TileOutcome::Present(_)
    ));
    assert_eq!(driver.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn export_pages_keep_their_generation_and_preserve_empty_positions() {
    let service = engine(Arc::new(Driver::new(
        "sample",
        "v1",
        Bytes::from(vec![5; 5 * 1024 * 1024]),
    )));
    let stack = TileStack::builder(service.clone()).build();
    let target = ReadTarget::new("sample", "public");
    let export = stack.begin_export(target).unwrap();
    let none = TileCoord { z: 1, x: 0, y: 1 };
    let coords = [none, ORIGIN, ORIGIN, ORIGIN];
    let first = export.read_page(&coords).await.unwrap();
    assert_eq!(first.consumed, 3);
    assert!(matches!(first.tiles[0], TileOutcome::NoTile));
    let (_, next) = stack.read_export_page(target, &coords, None).await.unwrap();
    let cursor: ExportCursor =
        serde_json::from_slice(&serde_json::to_vec(&next.unwrap()).unwrap()).unwrap();
    assert_eq!(cursor.position(), 3);
    let (last, end) = stack
        .read_export_page(target, &coords, Some(&cursor))
        .await
        .unwrap();
    assert_eq!(last.consumed, 1);
    assert!(end.is_none());
    assert_eq!(
        stack
            .read_export_page(ReadTarget::new("sample", "private"), &coords, Some(&cursor))
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::VersionChanged
    );
    let mut change = service.catalog().update();
    let stale = service.catalog().update();
    change
        .put(Arc::new(Driver::new(
            "sample",
            "v2",
            Bytes::from(vec![9; 9 * 1024 * 1024]),
        )))
        .unwrap();
    service.catalog().publish(change).unwrap();
    assert_eq!(
        stack
            .read_export_page(target, &coords, Some(&cursor))
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::VersionChanged
    );
    assert_eq!(
        service.catalog().publish(stale).unwrap_err().kind(),
        ErrorKind::VersionChanged
    );
    let second = export.read_page(&coords[first.consumed..]).await.unwrap();
    assert_eq!(second.tiles[0].tile().unwrap().data[0], 5);
    let new = stack.read_batch(target, &[ORIGIN, ORIGIN]).await.unwrap();
    assert_eq!(new.consumed, 1);
    assert_eq!(new.tiles[0].tile().unwrap().data[0], 9);
}

#[tokio::test]
async fn revoking_the_observed_generation_interrupts_its_read_without_revoking_its_replacement() {
    let old = Arc::new(Driver {
        hold: Some(Arc::new(Notify::new())),
        ..Driver::new("sample", "v1", Bytes::new())
    });
    let service = engine(old.clone());
    let before = service.catalog().snapshot();
    let stack = TileStack::builder(service.clone()).build();
    let req = ReadTarget::new("sample", "public").at(ORIGIN);
    let mut read = Box::pin(stack.read(req));
    while old.calls.load(Ordering::SeqCst) == 0 {
        tokio::select! { _=read.as_mut()=>panic!("driver did not wait"), _=tokio::task::yield_now()=>{} }
    }
    let mut change = service.catalog().update();
    change
        .put(Arc::new(Driver::new("sample", "v2", Bytes::new())))
        .unwrap();
    service.catalog().publish(change).unwrap();
    let revoke = service.catalog().revoke(
        &before,
        "sample",
        tokio::time::Instant::now() + std::time::Duration::from_secs(2),
    );
    let (read, closed) = tokio::join!(read, revoke);
    assert_eq!(read.unwrap_err().kind(), ErrorKind::Cancelled);
    closed.unwrap();
    assert!(matches!(
        stack.read(req).await.unwrap(),
        TileOutcome::Present(_)
    ));
}

#[tokio::test]
async fn shutdown_drains_retired_handles_even_while_metadata_snapshots_are_held() {
    let old = Arc::new(Driver::new("sample", "v1", Bytes::new()));
    let weak = Arc::downgrade(&old);
    let service = engine(old);
    let prior = service.catalog().snapshot();
    let foreign = engine(Arc::new(Driver::new("sample", "v1", Bytes::new())));
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    assert_eq!(
        foreign
            .catalog()
            .revoke(&prior, "sample", deadline)
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidInput
    );
    let mut update = service.catalog().update();
    update
        .put(Arc::new(Driver::new("sample", "v2", Bytes::new())))
        .unwrap();
    service.catalog().publish(update).unwrap();
    assert!(weak.upgrade().is_some());
    let stale = service.catalog().update();
    service.catalog().shutdown(deadline).await.unwrap();
    service.catalog().shutdown(deadline).await.unwrap();
    assert!(weak.upgrade().is_none());
    assert!(prior.describe("sample").is_some());
    assert_eq!(
        service.catalog().publish(stale).unwrap_err().kind(),
        ErrorKind::Cancelled
    );
    let stack = TileStack::builder(service).build();
    assert_eq!(
        stack
            .read(ReadTarget::new("sample", "public").at(ORIGIN))
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::Cancelled
    );
}

#[tokio::test]
async fn same_revision_replacement_does_not_wait_for_retired_driver() {
    let hold = Arc::new(Notify::new());
    let old = Arc::new(Driver {
        hold: Some(hold),
        ..Driver::new("sample", "v1", Bytes::new())
    });
    let service = engine(old.clone());
    let before = service.catalog().snapshot();
    let stack = TileStack::builder(service.clone()).build();
    let req = ReadTarget::new("sample", "public").at(ORIGIN);
    let mut first = Box::pin(stack.read(req));
    while old.calls.load(Ordering::SeqCst) == 0 {
        tokio::select! { _ = first.as_mut() => panic!("old read did not wait"), _ = tokio::task::yield_now() => {} }
    }
    let new = Arc::new(Driver::new("sample", "v1", Bytes::new()));
    let mut update = service.catalog().update();
    update.put(new.clone()).unwrap();
    service.catalog().publish(update).unwrap();
    let mut second = Box::pin(stack.read(req));
    let observed = match futures::poll!(second.as_mut()) {
        std::task::Poll::Ready(result) => result,
        std::task::Poll::Pending => {
            let revoke = service.catalog().revoke(
                &before,
                "sample",
                tokio::time::Instant::now() + std::time::Duration::from_secs(2),
            );
            let (old_result, closed, fresh_result) = tokio::join!(first, revoke, second);
            assert_eq!(old_result.unwrap_err().kind(), ErrorKind::Cancelled);
            closed.unwrap();
            fresh_result
        }
    };
    assert!(
        matches!(observed, Ok(TileOutcome::Present(_))),
        "{observed:?}"
    );
    assert_eq!(new.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn native_batch_cancellation_covers_output_validation() {
    use tokio_util::sync::CancellationToken;
    struct NativeCancel(CancellationToken);
    #[async_trait]
    impl TileDriver for NativeCancel {
        fn descriptor(&self) -> SourceDescriptor {
            let mut d = SourceDescriptor::new(
                "native",
                tile(Bytes::new()).info,
                ContentVersion::from_input(b"v1"),
            );
            d.capabilities.batch = true;
            d
        }
        async fn read(
            &self,
            _: TileCoord,
            _: Option<&TileQuery<'_>>,
        ) -> MaptileResult<Option<TileResponse>> {
            unreachable!()
        }
        async fn read_batch(
            &self,
            _: &[TileCoord],
            _: Option<&TileQuery<'_>>,
        ) -> MaptileResult<Option<Vec<Option<TileResponse>>>> {
            let bytes = maptile_core::transport::encode_as(b"valid tile", TileEncoding::Gzip)?;
            let mut row = tile(bytes);
            row.info.encoding = TileEncoding::Gzip;
            self.0.cancel();
            Ok(Some(vec![Some(row)]))
        }
    }
    let cancel = CancellationToken::new();
    let engine = Arc::new(CatalogService::new(TileCache::new(1, None, None).unwrap()));
    let mut edit = engine.catalog().update();
    edit.put(Arc::new(NativeCancel(cancel.clone()))).unwrap();
    engine.catalog().publish(edit).unwrap();
    let stack = TileStack::builder(engine).build();
    let req = ReadTarget {
        cancellation: Some(&cancel),
        ..ReadTarget::new("native", "scope")
    };
    let result = stack.read_batch(req, &[ORIGIN]).await;
    assert!(matches!(result, Err(MaptileError::Cancelled)), "{result:?}");
}
