//! Tests for `PostgresPool` — require a live PostGIS database via `DATABASE_URL`.

#[path = "../../common/mod.rs"]
mod common;

use maptile_core::sources::postgres::{
    MINIMUM_POSTGIS_VERSION, MINIMUM_POSTGRES_VERSION, PoolSettings, PostgresPool,
};

static BATCH_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn default_settings() -> PoolSettings {
    PoolSettings {
        max_size: 4,
        wait_timeout_ms: 5_000,
        create_timeout_ms: 5_000,
        recycle_timeout_ms: 5_000,
        pre_warm: false,
    }
}

#[tokio::test]
async fn pool_connects_and_reports_versions() {
    let dsn = match common::postgres::pg_dsn() {
        Some(d) => d,
        None => return,
    };
    let pool = PostgresPool::connect(&dsn, default_settings())
        .await
        .unwrap();
    assert!(pool.postgres_version() >= &MINIMUM_POSTGRES_VERSION);
    assert!(pool.postgis_version() >= &MINIMUM_POSTGIS_VERSION);
    // tile_margin is reported truthfully — sanity check that the boolean
    // matches the version comparison.
    assert_eq!(
        pool.supports_tile_margin(),
        pool.postgis_version() >= &semver::Version::new(3, 1, 0)
    );
}

#[tokio::test]
async fn pool_can_acquire_and_query() {
    let dsn = match common::postgres::pg_dsn() {
        Some(d) => d,
        None => return,
    };
    let pool = PostgresPool::connect(&dsn, default_settings())
        .await
        .unwrap();
    let conn = pool.get().await.unwrap();
    let row = conn.query_one("SELECT 1::int", &[]).await.unwrap();
    let v: i32 = row.get(0);
    assert_eq!(v, 1);
    assert_eq!(
        conn.query_one("SHOW jit", &[])
            .await
            .unwrap()
            .get::<_, String>(0),
        "off"
    );
}

#[tokio::test]
async fn prewarmed_and_replacement_connections_keep_startup_settings() {
    let Some(dsn) = common::postgres::pg_dsn() else {
        return;
    };
    let name = common::postgres::unique_schema();
    let dsn = if dsn.starts_with("postgres://") || dsn.starts_with("postgresql://") {
        let separator = if dsn.contains('?') { '&' } else { '?' };
        format!("{dsn}{separator}options=-c%20application_name%3D{name}%20-c%20jit%3Don")
    } else {
        format!("{dsn} options='-c application_name={name} -c jit=on'")
    };
    let pool = PostgresPool::connect(
        &dsn,
        PoolSettings {
            pre_warm: true,
            ..default_settings()
        },
    )
    .await
    .unwrap();
    let first = pool.get().await.unwrap();
    let count: i64 = first
        .query_one(
            "SELECT count(*) FROM pg_stat_activity WHERE application_name=$1",
            &[&name],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 4, "pre_warm must create distinct connections");
    let pid: i32 = first
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    drop(first);
    let first = pool.get().await.unwrap();
    assert_eq!(
        first
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get::<_, i32>(0),
        pid,
        "prefer the connection with a hot statement cache"
    );
    let mut held = vec![first];
    for _ in 1..4 {
        held.push(pool.get().await.unwrap());
    }
    for conn in &held {
        let row = conn
            .query_one(
                "SELECT current_setting('jit'),current_setting('application_name')",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(row.get::<_, String>(0), "off");
        assert_eq!(row.get::<_, String>(1), name);
    }
    drop(deadpool_postgres::Object::take(held.pop().unwrap()));
    let replacement = pool.get().await.unwrap();
    assert_eq!(
        replacement
            .query_one("SHOW jit", &[])
            .await
            .unwrap()
            .get::<_, String>(0),
        "off"
    );
}

#[tokio::test]
async fn pool_rejects_unreachable_dsn() {
    // Point at a port nothing listens on; expect a Backend error quickly.
    let bad = "host=127.0.0.1 port=1 user=postgres dbname=postgres connect_timeout=2";
    let res = PostgresPool::connect(bad, default_settings()).await;
    assert!(
        res.is_err(),
        "expected connect failure, got: {:?}",
        res.ok()
    );
}

#[tokio::test]
async fn batch_sql_admission_survives_a_controlled_cancellation_storm() {
    let _batch_test = BATCH_TEST_LOCK.lock().await;
    use maptile_core::sources::{
        TileCoord, TileQuery, TileSource,
        postgres::{
            PostgresFunctionConfig, PostgresFunctionSource, PostgresTableConfig,
            PostgresTableSource,
        },
    };
    use std::{sync::Arc, time::Duration};
    let Some(dsn) = common::postgres::pg_dsn() else {
        return;
    };
    let schema = common::postgres::SchemaGuard::create(&dsn).await;
    let name = format!("{}_batch", schema.name);
    let named_dsn = if dsn.starts_with("postgres://") || dsn.starts_with("postgresql://") {
        format!(
            "{dsn}{}application_name={name}",
            if dsn.contains('?') { '&' } else { '?' }
        )
    } else {
        format!("{dsn} application_name={name}")
    };
    let (monitor, connection) = tokio_postgres::connect(&dsn, tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    monitor.batch_execute(&format!(r#"
        CREATE TABLE "{s}".control(enabled boolean);
        INSERT INTO "{s}".control VALUES(true);
        CREATE SEQUENCE "{s}".started;
        CREATE FUNCTION "{s}".slow_name(value text) RETURNS text LANGUAGE plpgsql VOLATILE AS $$
        BEGIN
          PERFORM nextval('{s}.started');
          IF (SELECT enabled FROM "{s}".control) THEN
            BEGIN PERFORM pg_sleep(10);
            EXCEPTION WHEN query_canceled THEN PERFORM pg_sleep(0.15); END;
          END IF;
          RETURN value;
        END $$;
        CREATE TABLE "{s}".point(id bigint,name text,geom geometry(Point,4326));
        INSERT INTO "{s}".point VALUES(1,'point',ST_SetSRID(ST_Point(0,0),4326));
        CREATE VIEW "{s}".storm_batch AS SELECT id,"{s}".slow_name(name) AS name,geom FROM "{s}".point;
        CREATE FUNCTION "{s}".storm_function(z integer,x integer,y integer) RETURNS bytea LANGUAGE plpgsql VOLATILE AS $$
        BEGIN PERFORM "{s}".slow_name('point'); RETURN decode('1a0c0a05726f6164732880207802','hex'); END $$;
    "#,s=schema.name)).await.unwrap();
    let table_pool = Arc::new(
        PostgresPool::connect(&named_dsn, default_settings())
            .await
            .unwrap(),
    );
    let function_pool = Arc::new(
        PostgresPool::connect(&named_dsn, default_settings())
            .await
            .unwrap(),
    );
    let mut config = PostgresTableConfig::new("storm-table", &schema.name, "storm_batch");
    config.bounds = Some([-180., -85., 180., 85.]);
    config.id_column = Some("id".into());
    config.properties = Some(vec!["name".into()]);
    let table: Arc<dyn TileSource> = Arc::new(
        PostgresTableSource::open(table_pool.clone(), config)
            .await
            .unwrap()
            .with_allowed_columns(Arc::new(std::collections::HashSet::from(["id".into()]))),
    );
    let function: Arc<dyn TileSource> = Arc::new(
        PostgresFunctionSource::open(
            function_pool,
            PostgresFunctionConfig {
                id: "storm-function".into(),
                schema: schema.name.clone(),
                function: "storm_function".into(),
                description: None,
                minzoom: None,
                maxzoom: None,
                bounds: None,
                cache_zoom: None,
            },
        )
        .await
        .unwrap(),
    );
    let manager = Arc::new(maptile_core::sources::manager::SourceManager::new());
    manager.register(table.clone());
    manager.register(function.clone());
    let composite: Arc<dyn TileSource> = Arc::new(
        maptile_core::sources::CompositeSource::new(
            "storm-composite",
            vec![
                maptile_core::sources::CompositeChild::new(table.clone()),
                maptile_core::sources::CompositeChild::new(function.clone()),
            ],
        )
        .unwrap(),
    );
    let lazy: Arc<dyn TileSource> = Arc::new(maptile_core::sources::LazyCompositeSource::open(
        "storm-group".into(),
        vec![table.id().into(), function.id().into()],
        Arc::downgrade(&manager),
    ));
    monitor
        .query_one(
            &format!("SELECT setval('{}.started',1,false)", schema.name),
            &[],
        )
        .await
        .unwrap();
    let admission = Arc::new(tokio::sync::Semaphore::new(2));
    let spawn = |source: Arc<dyn TileSource>, filtered: bool| {
        let admission = admission.clone();
        tokio::spawn(async move {
            let _permit = admission.acquire_owned().await.unwrap();
            let expr = filtered.then(|| maptile_core::filter::parse_cql2("id=1").unwrap());
            let query = expr.as_ref().map(|expr| TileQuery {
                filter: Some(expr),
                ..TileQuery::default()
            });
            source
                .read_tile_batch(
                    &[TileCoord {
                        z: 14,
                        x: 8192,
                        y: 8192,
                    }],
                    query.as_ref(),
                )
                .await
        })
    };
    let mut current = spawn(table.clone(), false);
    let persistent = spawn(function.clone(), false);
    let mut peak = 0_i64;
    let mut starved = false;
    let mut expected = 2_i64;
    for replacement in 0..=8 {
        if replacement > 0 {
            current.abort();
            let _ = current.await;
            expected = monitor
                .query_one(
                    &format!("SELECT last_value FROM \"{}\".started", schema.name),
                    &[],
                )
                .await
                .unwrap()
                .get::<_, i64>(0)
                + 1;
            let (source, filtered) = match replacement % 5 {
                0 => (table.clone(), false),
                1 => (table.clone(), true),
                2 => (function.clone(), false),
                3 => (composite.clone(), false),
                _ => (lazy.clone(), false),
            };
            current = spawn(source, filtered);
        }
        let started=tokio::time::timeout(Duration::from_secs(3),async {
            loop {
                let row=monitor.query_one(&format!("SELECT (SELECT count(*) FROM pg_stat_activity WHERE application_name=$1 AND state='active'),(SELECT last_value FROM \"{}\".started)",schema.name),&[&name]).await.unwrap();
                peak=peak.max(row.get::<_,i64>(0));
                if row.get::<_,i64>(1)>=expected { break; }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }).await;
        if started.is_err() {
            starved = true;
            break;
        }
    }
    current.abort();
    persistent.abort();
    let _ = current.await;
    let _ = persistent.await;
    monitor
        .batch_execute(&format!(
            "UPDATE \"{}\".control SET enabled=false",
            schema.name
        ))
        .await
        .unwrap();
    let stopped=tokio::time::timeout(Duration::from_secs(2),async {
        loop {
            let row=monitor.query_one("SELECT count(*) FROM pg_stat_activity WHERE application_name=$1 AND state='active'",&[&name]).await.unwrap();
            peak=peak.max(row.get::<_,i64>(0));
            if row.get::<_,i64>(0)==0 { break; }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }).await;
    let restored = tokio::time::timeout(
        Duration::from_secs(2),
        futures_util::future::try_join_all([
            table.read_tile_batch(
                &[TileCoord {
                    z: 14,
                    x: 8192,
                    y: 8192,
                }],
                None,
            ),
            function.read_tile_batch(
                &[TileCoord {
                    z: 14,
                    x: 8192,
                    y: 8192,
                }],
                None,
            ),
            composite.read_tile_batch(
                &[TileCoord {
                    z: 14,
                    x: 8192,
                    y: 8192,
                }],
                None,
            ),
            lazy.read_tile_batch(
                &[TileCoord {
                    z: 14,
                    x: 8192,
                    y: 8192,
                }],
                None,
            ),
        ]),
    )
    .await;
    monitor.query("SELECT pg_cancel_backend(pid) FROM pg_stat_activity WHERE application_name=$1 AND state='active'",&[&name]).await.unwrap();
    println!(
        "SQL_ADMISSION peak={peak} replacements=8 starved={starved} stopped={} restored={}",
        stopped.is_ok(),
        restored.as_ref().is_ok_and(|r| r.is_ok())
    );
    assert!(
        peak <= 2,
        "actual PostgreSQL batch concurrency reached {peak}"
    );
    assert!(!starved, "a replacement batch never acquired its SQL slot");
    assert!(stopped.is_ok(), "cancelled SQL did not finish cleanup");
    assert!(
        restored.unwrap().is_ok(),
        "SQL admission slots were not restored"
    );
}

#[tokio::test]
async fn batch_admission_waits_for_twelve_second_backend_cleanup() {
    let _batch_test = BATCH_TEST_LOCK.lock().await;
    use maptile_core::sources::{
        TileCoord, TileSource,
        postgres::{PostgresFunctionConfig, PostgresFunctionSource},
    };
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };
    let Some(dsn) = common::postgres::pg_dsn() else {
        return;
    };
    let schema = common::postgres::SchemaGuard::create(&dsn).await;
    let name = format!("{}_slow_cleanup", schema.name);
    let named_dsn = if dsn.starts_with("postgres://") || dsn.starts_with("postgresql://") {
        format!(
            "{dsn}{}application_name={name}",
            if dsn.contains('?') { '&' } else { '?' }
        )
    } else {
        format!("{dsn} application_name={name}")
    };
    let (monitor, connection) = tokio_postgres::connect(&dsn, tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    assert_eq!(
        monitor
            .query_one("SHOW client_connection_check_interval", &[])
            .await
            .unwrap()
            .get::<_, String>(0),
        "0"
    );
    monitor.batch_execute(&format!(r#"
        CREATE SEQUENCE "{s}".started;
        CREATE SEQUENCE "{s}".replacement_started;
        CREATE FUNCTION "{s}".slow_cleanup(z integer,x integer,y integer) RETURNS bytea LANGUAGE plpgsql AS $$
        BEGIN
          IF x=0 THEN
            PERFORM nextval('{s}.started');
            BEGIN PERFORM pg_sleep(30);
            EXCEPTION WHEN query_canceled THEN PERFORM pg_sleep(12); END;
          ELSIF x=1 THEN
            PERFORM nextval('{s}.replacement_started'); PERFORM pg_sleep(1);
          ELSIF x=3 THEN
            RAISE division_by_zero;
          END IF;
          RETURN decode('1a0c0a05726f6164732880207802','hex');
        END $$;
    "#,s=schema.name)).await.unwrap();
    let pool = Arc::new(
        PostgresPool::connect(&named_dsn, default_settings())
            .await
            .unwrap(),
    );
    let source = Arc::new(
        PostgresFunctionSource::open(
            pool,
            PostgresFunctionConfig {
                id: "slow-cleanup".into(),
                schema: schema.name.clone(),
                function: "slow_cleanup".into(),
                description: None,
                minzoom: None,
                maxzoom: None,
                bounds: None,
                cache_zoom: None,
            },
        )
        .await
        .unwrap(),
    );
    let spawn = |x| {
        let source = source.clone();
        tokio::spawn(async move {
            source
                .read_tile_batch(&[TileCoord { z: 2, x, y: 0 }], None)
                .await
        })
    };
    let first = [spawn(0), spawn(0)];
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let row = monitor
                .query_one(
                    &format!(
                        "SELECT last_value,is_called FROM \"{}\".started",
                        schema.name
                    ),
                    &[],
                )
                .await
                .unwrap();
            if row.get::<_, bool>(1) && row.get::<_, i64>(0) == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let online = tokio::time::timeout(
        Duration::from_secs(1),
        source.get_tile(TileCoord { z: 2, x: 2, y: 0 }, None),
    )
    .await;
    for job in &first {
        job.abort();
    }
    for job in first {
        let _ = job.await;
    }
    let started = Instant::now();
    let next = [spawn(1), spawn(1)];
    let mut peak = 0_i64;
    let mut first_replacement = None;
    let stopped=tokio::time::timeout(Duration::from_secs(18),async {
        loop {
            let row=monitor.query_one(&format!("SELECT (SELECT count(*) FROM pg_stat_activity WHERE application_name=$1 AND state='active'),(SELECT is_called FROM \"{}\".replacement_started)",schema.name),&[&name]).await.unwrap();
            let active=row.get::<_,i64>(0);
            peak=peak.max(active);
            if row.get::<_,bool>(1) && first_replacement.is_none() { first_replacement=Some(started.elapsed()); }
            if active==0 && next.iter().all(tokio::task::JoinHandle::is_finished) { break; }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }).await;
    if stopped.is_err() {
        for job in &next {
            job.abort();
        }
        monitor
            .query(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE application_name=$1",
                &[&name],
            )
            .await
            .unwrap();
    }
    let mut recovered = stopped.is_ok();
    for job in next {
        recovered &= job.await.is_ok_and(|r| r.is_ok());
    }
    let error = source
        .read_tile_batch(&[TileCoord { z: 2, x: 3, y: 0 }], None)
        .await;
    let after_error = tokio::time::timeout(
        Duration::from_secs(2),
        source.read_tile_batch(&[TileCoord { z: 2, x: 2, y: 0 }], None),
    )
    .await;
    println!(
        "SLOW_CLEANUP peak={peak} replacement_after_seconds={:.3} restored={recovered} online={} error_recovered={}",
        first_replacement.unwrap_or_default().as_secs_f64(),
        online.as_ref().is_ok_and(|r| r.is_ok()),
        after_error.as_ref().is_ok_and(|r| r.is_ok())
    );
    assert!(
        peak <= 2,
        "actual PostgreSQL batch concurrency reached {peak}"
    );
    assert!(
        first_replacement.unwrap() >= Duration::from_secs(11),
        "backend cleanup was still active when replacement entered"
    );
    assert!(recovered, "SQL slots did not recover after ReadyForQuery");
    assert!(
        online.unwrap().is_ok(),
        "ordinary online get_tile was blocked by archive admission"
    );
    assert!(error.is_err());
    assert!(
        after_error.unwrap().is_ok(),
        "ordinary SQL error closed healthy archive admission"
    );
}

#[test]
fn batch_transport_loss_stops_archive_admission_but_keeps_online_reads() {
    let Some(dsn) = common::postgres::pg_dsn() else {
        return;
    };
    // Fail-closed admission is process-wide; isolate the destructive fault from other tests.
    if std::env::var_os("MAPTILE_BATCH_TRANSPORT_CHILD").is_none() {
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "batch_transport_loss_stops_archive_admission_but_keeps_online_reads",
                "--nocapture",
            ])
            .env("MAPTILE_BATCH_TRANSPORT_CHILD", "1")
            .status()
            .unwrap();
        assert!(result.success(), "transport fault subprocess failed");
        return;
    }
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(assert_batch_transport_loss(dsn));
}

async fn assert_batch_transport_loss(dsn: String) {
    use maptile_core::sources::{
        TileCoord, TileSource,
        postgres::{PostgresFunctionConfig, PostgresFunctionSource},
    };
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Duration,
    };
    let schema = common::postgres::SchemaGuard::create(&dsn).await;
    let name = format!("{}_lost", schema.name);
    let (monitor, connection) = tokio_postgres::connect(&dsn, tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    monitor.batch_execute(&format!(r#"
            CREATE SEQUENCE "{s}".started;
            CREATE FUNCTION "{s}".transport_batch(z integer,x integer,y integer) RETURNS bytea LANGUAGE plpgsql AS $$
            BEGIN
              IF x=0 OR x=2 THEN PERFORM nextval('{s}.started'); IF x=0 THEN PERFORM pg_sleep(30); ELSE PERFORM pg_sleep(1); END IF; END IF;
              RETURN decode('1a0c0a05726f6164732880207802','hex');
            END $$;
        "#,s=schema.name)).await.unwrap();
    let config = dsn.parse::<tokio_postgres::Config>().unwrap();
    let tokio_postgres::config::Host::Tcp(host) = &config.get_hosts()[0] else {
        panic!("fault fixture requires TCP PostGIS");
    };
    let endpoint = (
        host.clone(),
        config.get_ports().first().copied().unwrap_or(5432),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_port = listener.local_addr().unwrap().port();
    let broken = Arc::new(AtomicBool::new(false));
    let reject_cancel = Arc::new(AtomicUsize::new(0));
    let fault = Arc::new(tokio::sync::Notify::new());
    let proxy = {
        let broken = broken.clone();
        let reject_cancel = reject_cancel.clone();
        let fault = fault.clone();
        tokio::spawn(async move {
            let mut sockets = tokio::task::JoinSet::new();
            loop {
                let (mut client, _) = listener.accept().await.unwrap();
                if broken.load(Ordering::SeqCst)
                    || reject_cancel
                        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                        .is_ok()
                {
                    drop(client);
                    continue;
                }
                let mut server = tokio::net::TcpStream::connect(&endpoint).await.unwrap();
                let fault = fault.clone();
                sockets.spawn(async move {
                    tokio::select! {
                        _=tokio::io::copy_bidirectional(&mut client,&mut server)=>{},
                        _=fault.notified()=>{
                            drop(client);
                            // Keep the backend transport open while the client loses all status information.
                            tokio::time::sleep(Duration::from_secs(30)).await;
                            drop(server);
                        }
                    }
                });
            }
        })
    };
    let quote = |value: &str| format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"));
    let proxy_dsn = format!(
        "host=127.0.0.1 port={proxy_port} user={} password={} dbname={} application_name={name} sslmode=disable",
        quote(config.get_user().unwrap()),
        quote(std::str::from_utf8(config.get_password().unwrap_or_default()).unwrap()),
        quote(config.get_dbname().unwrap_or("postgres"))
    );
    let pool = Arc::new(
        PostgresPool::connect(&proxy_dsn, default_settings())
            .await
            .unwrap(),
    );
    let source_config = PostgresFunctionConfig {
        id: "transport-batch".into(),
        schema: schema.name.clone(),
        function: "transport_batch".into(),
        description: None,
        minzoom: None,
        maxzoom: None,
        bounds: None,
        cache_zoom: None,
    };
    let source = Arc::new(
        PostgresFunctionSource::open(pool, source_config.clone())
            .await
            .unwrap(),
    );
    let mut cancelled = Vec::new();
    for _ in 0..2 {
        let source = source.clone();
        cancelled.push(tokio::spawn(async move {
            source
                .read_tile_batch(&[TileCoord { z: 2, x: 2, y: 0 }], None)
                .await
        }));
    }
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let row = monitor
                .query_one(
                    &format!(
                        "SELECT last_value,is_called FROM \"{}\".started",
                        schema.name
                    ),
                    &[],
                )
                .await
                .unwrap();
            if row.get::<_, bool>(1) && row.get::<_, i64>(0) == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    reject_cancel.store(2, Ordering::SeqCst);
    for job in &cancelled {
        job.abort();
    }
    for job in cancelled {
        let _ = job.await;
    }
    let recovered = tokio::time::timeout(
        Duration::from_secs(3),
        source.read_tile_batch(&[TileCoord { z: 1, x: 1, y: 0 }], None),
    )
    .await;
    println!(
        "CANCEL_DELIVERY_LOSS discarded={} ready_recovered={}",
        2 - reject_cancel.load(Ordering::SeqCst),
        recovered.as_ref().is_ok_and(|r| r.is_ok())
    );
    assert_eq!(
        reject_cancel.load(Ordering::SeqCst),
        0,
        "both cancellation connections must fail"
    );
    assert!(
        recovered.unwrap().is_ok(),
        "cancel failure closed admission despite healthy ReadyForQuery"
    );
    monitor
        .batch_execute(&format!("SELECT setval('{}.started',1,false)", schema.name))
        .await
        .unwrap();
    let mut jobs = Vec::new();
    for _ in 0..2 {
        let source = source.clone();
        jobs.push(tokio::spawn(async move {
            source
                .read_tile_batch(&[TileCoord { z: 1, x: 0, y: 0 }], None)
                .await
        }));
    }
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let row = monitor
                .query_one(
                    &format!(
                        "SELECT last_value,is_called FROM \"{}\".started",
                        schema.name
                    ),
                    &[],
                )
                .await
                .unwrap();
            if row.get::<_, bool>(1) && row.get::<_, i64>(0) == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    broken.store(true, Ordering::SeqCst);
    fault.notify_waiters();
    for job in jobs {
        assert!(job.await.unwrap().is_err());
    }
    let mut closed = false;
    let mut peak = 0_i64;
    for _ in 0..2 {
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            source.read_tile_batch(&[TileCoord { z: 1, x: 1, y: 0 }], None),
        )
        .await;
        closed |= result.as_ref().is_ok_and(|r| {
            r.as_ref()
                .is_err_and(|e| e.to_string().contains("archive SQL admission closed"))
        });
        peak=peak.max(monitor.query_one("SELECT count(*) FROM pg_stat_activity WHERE application_name=$1 AND state='active'",&[&name]).await.unwrap().get::<_,i64>(0));
        assert!(
            result.unwrap().is_err(),
            "unknown old backends allowed new archive SQL"
        );
    }
    let online_pool = Arc::new(
        PostgresPool::connect(&dsn, default_settings())
            .await
            .unwrap(),
    );
    let online_source = PostgresFunctionSource::open(online_pool, source_config)
        .await
        .unwrap();
    let online = tokio::time::timeout(
        Duration::from_secs(1),
        online_source.get_tile(TileCoord { z: 1, x: 1, y: 0 }, None),
    )
    .await;
    monitor
        .query(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE application_name=$1",
            &[&name],
        )
        .await
        .unwrap();
    proxy.abort();
    let _ = proxy.await;
    println!(
        "TRANSPORT_LOSS old_active={peak} archive_closed={closed} online={}",
        online.as_ref().is_ok_and(|r| r.is_ok())
    );
    assert_eq!(peak, 2, "fixture must leave both old server queries active");
    assert!(
        closed,
        "archive failure did not identify the stopped admission gate"
    );
    assert!(
        online.unwrap().is_ok(),
        "online get_tile consulted archive admission"
    );
}
