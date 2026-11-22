use async_trait::async_trait;
use bytes::Bytes;
use maptile_core::{
    cache::{ContentVersion, StyleHash, TileCache},
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
const MVT: TileInfo = TileInfo {
    format: TileFormat::Mvt,
    encoding: TileEncoding::Uncompressed,
};
const GZIP_MVT: TileInfo = TileInfo {
    encoding: TileEncoding::Gzip,
    ..MVT
};

fn tile(bytes: Bytes) -> TileResponse {
    TileResponse {
        data: bytes,
        info: MVT,
        last_modified: None,
    }
}

struct Driver {
    name: &'static str,
    revision: &'static str,
    bytes: Bytes,
    info: TileInfo,
    raster_style: bool,
    calls: AtomicUsize,
    hold: Option<Arc<Notify>>,
}
impl Driver {
    fn new(name: &'static str, revision: &'static str, data: Bytes) -> Self {
        Self {
            name,
            revision,
            bytes: data,
            info: MVT,
            raster_style: false,
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
            self.info,
            ContentVersion::from_input(self.revision.as_bytes()),
        );
        d.capabilities.consistent_export = true;
        d.capabilities.raster_style = self.raster_style;
        d
    }
    async fn read(
        &self,
        coord: TileCoord,
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<Option<TileResponse>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(hold) = &self.hold {
            hold.notified().await;
        }
        Ok((coord.y == 0).then(|| {
            let data = if self.raster_style {
                let query = query.unwrap();
                let size = query.style.unwrap().tile_size as u8;
                let hash = query.style_hash.unwrap();
                Bytes::copy_from_slice(&[size, hash.as_bytes()[0]])
            } else {
                self.bytes.clone()
            };
            TileResponse {
                data,
                info: self.info,
                last_modified: None,
            }
        }))
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

struct ReplaceAfterRead(TileResponse);
#[async_trait]
impl TileLayer for ReplaceAfterRead {
    async fn read(
        &self,
        _: &ValidatedTileRequest<'_>,
        next: TileService<'_>,
    ) -> MaptileResult<TileOutcome> {
        next.read().await?;
        Ok(TileOutcome::Present(self.0.clone()))
    }
}

fn oversized_gzip_tile() -> TileResponse {
    use std::io::{self, Read};
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    io::copy(
        &mut io::repeat(0).take(maptile_core::transport::MAX_DECODED_TILE_BYTES as u64 + 1),
        &mut encoder,
    )
    .unwrap();
    TileResponse {
        info: GZIP_MVT,
        ..tile(Bytes::from(encoder.finish().unwrap()))
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
    for invalid in [oversized_gzip_tile(), tile(Bytes::from(data))] {
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

#[test]
fn compressed_hot_tile_passes_through_layers_without_a_blocking_worker() {
    use maptile_core::cache::{CACHE_SCHEMA_VERSION, TileCacheKey, hashing::canonical_render_hash};
    use std::time::Duration;
    use tokio::sync::oneshot;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let data = maptile_core::transport::encode_as(b"canonical", TileEncoding::Gzip).unwrap();
        let driver = Arc::new(Driver {
            info: GZIP_MVT,
            ..Driver::new("sample", "v1", data.clone())
        });
        let cache = TileCache::new(1, None, None).unwrap();
        let service = Arc::new(CatalogService::new(cache.clone()));
        let mut edit = service.catalog().update();
        edit.put(driver.clone()).unwrap();
        service.catalog().publish(edit).unwrap();
        let snapshot = service.catalog().snapshot();
        let descriptor = snapshot.describe("sample").unwrap();
        let mut identity = blake3::Hasher::new();
        identity.update(&CACHE_SCHEMA_VERSION.to_le_bytes());
        identity.update(&hex::decode(descriptor.revision.to_hex()).unwrap());
        identity.update(b"public");
        let key = TileCacheKey::new(
            descriptor.id.clone(),
            ContentVersion::from_bytes(*identity.finalize().as_bytes()),
            ORIGIN.z,
            ORIGIN.x,
            ORIGIN.y,
            canonical_render_hash(None, None, 256, 1, 0),
        );
        let req = ReadTarget::new("sample", "public").at(ORIGIN);
        let plain = TileStack::builder(service.clone()).build();
        let stack = TileStack::builder(service)
            .layer(Record("outer", Arc::default()))
            .layer(Record("inner", Arc::default()))
            .build();
        stack.read(req).await.unwrap();
        assert!(cache.get(&key).await.is_some());

        let seeded =
            maptile_core::transport::encode_as(b"public cache write", TileEncoding::Gzip).unwrap();
        for (expected, seed) in [(&data, false), (&seeded, true)] {
            if seed {
                cache
                    .insert(
                        key.clone(),
                        TileResponse {
                            info: GZIP_MVT,
                            ..tile(seeded.clone())
                        },
                    )
                    .await;
                let cold = stack.read(req).await.unwrap();
                assert_eq!(&cold.tile().unwrap().data, expected);
            }
            let (started, ready) = oneshot::channel();
            let (release, done) = oneshot::channel();
            let occupied = tokio::task::spawn_blocking(move || {
                started.send(()).unwrap();
                let _ = done.blocking_recv();
            });
            ready.await.unwrap();
            let warm = tokio::time::timeout(Duration::from_secs(2), stack.read(req)).await;
            release.send(()).unwrap();
            occupied.await.unwrap();
            let outcome = warm.expect("warm read started a second decode").unwrap();
            assert_eq!(&outcome.tile().unwrap().data, expected);
        }

        cache.insert(key, oversized_gzip_tile()).await;
        for stack in [plain, stack] {
            assert_eq!(
                stack.read(req).await.unwrap_err().kind(),
                ErrorKind::ResourceLimit
            );
        }
        assert_eq!(driver.calls.load(Ordering::SeqCst), 1);
    });
}

#[tokio::test]
async fn compressed_fills_and_changed_layer_outputs_are_checked() {
    let invalid = oversized_gzip_tile();
    let driver = Arc::new(Driver {
        info: GZIP_MVT,
        ..Driver::new("invalid", "v1", invalid.data.clone())
    });
    let stack = TileStack::builder(engine(driver.clone())).build();
    for _ in 0..2 {
        assert!(matches!(
            stack
                .read(ReadTarget::new("invalid", "public").at(ORIGIN))
                .await,
            Err(MaptileError::PayloadTooLarge { .. })
        ));
    }
    assert_eq!(driver.calls.load(Ordering::SeqCst), 2);

    let data = maptile_core::transport::encode_as(b"canonical", TileEncoding::Gzip).unwrap();
    let service = engine(Arc::new(Driver {
        info: GZIP_MVT,
        ..Driver::new("sample", "v1", data.clone())
    }));
    for (scope, replacement) in [
        ("different-bytes", invalid),
        (
            "shortened-view",
            TileResponse {
                data: data.slice(..data.len() - 1),
                info: GZIP_MVT,
                last_modified: None,
            },
        ),
        (
            "different-encoding",
            TileResponse {
                data: data.clone(),
                info: TileInfo {
                    encoding: TileEncoding::Zlib,
                    ..MVT
                },
                last_modified: None,
            },
        ),
    ] {
        let stack = TileStack::builder(service.clone())
            .layer(ReplaceAfterRead(replacement))
            .build();
        let error = stack
            .read(ReadTarget::new("sample", scope).at(ORIGIN))
            .await
            .unwrap_err();
        if scope == "different-bytes" {
            assert!(matches!(error, MaptileError::PayloadTooLarge { .. }));
        } else {
            assert!(matches!(error, MaptileError::Transcode(_)));
        }
    }
}

#[tokio::test]
async fn custom_driver_can_apply_raster_style_independent_of_advertised_tile_format() {
    use maptile_core::raster::style::EffectiveStyle;

    let driver = Arc::new(Driver {
        raster_style: true,
        ..Driver::new("sample", "v1", Bytes::new())
    });
    let stack = TileStack::builder(engine(driver.clone())).build();
    let document = raster_style::Style::from_value(serde_json::json!({
        "version": "3.0", "extensions": [raster_style::TILE_SCHEMA_URL],
        "renderer": {"type": "gray", "bidx": [1]},
        "raster:tile": {"format": "png", "tilesize": 128}
    }))
    .unwrap();
    let style = EffectiveStyle::from_raster_style(document).unwrap();
    for (tag, expected_calls) in [(3, 1), (3, 1), (7, 2)] {
        let query = TileQuery {
            style: Some(&style),
            style_hash: Some(StyleHash::from_bytes([tag; 32])),
            ..Default::default()
        };
        let target = ReadTarget {
            query: Some(&query),
            ..ReadTarget::new("sample", "public")
        };
        let page = stack.read_batch(target, &[ORIGIN]).await.unwrap();
        let tile = page.tiles[0].tile().unwrap();
        assert_eq!(tile.info.format, TileFormat::Mvt);
        assert_eq!(tile.data.as_ref(), &[128, tag]);
        assert_eq!(driver.calls.load(Ordering::SeqCst), expected_calls);
    }
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
    let query = TileQuery {
        style_hash: Some(StyleHash::from_bytes([7; 32])),
        ..Default::default()
    };
    let styled = ReadTarget {
        query: Some(&query),
        ..req
    };
    assert!(matches!(
        stack.read_batch(styled, &[ORIGIN]).await,
        Err(MaptileError::Unsupported(_))
    ));
    assert!(!cancel.is_cancelled());
    let result = stack.read_batch(req, &[ORIGIN]).await;
    assert!(matches!(result, Err(MaptileError::Cancelled)), "{result:?}");
}
