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
        batch_concurrency: 2,
        wait_timeout_ms: 5_000,
        create_timeout_ms: 5_000,
        recycle_timeout_ms: 5_000,
        pre_warm: false,
    }
}

#[tokio::test]
async fn shutdown_waits_for_a_cancelled_factory_pool_connection() {
    use maptile_core::{
        cache::ContentVersion,
        control::{DiscoveredDataset, Inventory, SourceCatalog},
        error::MaptileError,
    };
    use std::{sync::Arc, time::Duration};
    use tokio::{io::AsyncReadExt, net::TcpListener, time};

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let dsn = format!("host=127.0.0.1 port={port} user=catalog dbname=catalog sslmode=disable");
    let catalog = Arc::new(SourceCatalog::default());
    let opening = tokio::spawn({
        let catalog = catalog.clone();
        async move {
            let items = [DiscoveredDataset {
                id: "roads".into(),
                dataset: "roads".into(),
                revision: ContentVersion::from_input(b"v1"),
            }];
            catalog
                .reconcile(0, "postgres", Inventory::Complete(&items), |_| async {
                    PostgresPool::connect(&dsn, default_settings()).await?;
                    Err(MaptileError::Config(
                        "fixture must stop during startup".into(),
                    ))
                })
                .await
        }
    });

    let deadline = time::Instant::now() + Duration::from_secs(5);
    let (mut socket, _) = time::timeout_at(deadline, listener.accept())
        .await
        .unwrap()
        .unwrap();
    let mut startup = [0_u8; 8];
    time::timeout_at(deadline, socket.read_exact(&mut startup))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&startup[4..], &196608_u32.to_be_bytes()); // PostgreSQL protocol 3.0
    opening.abort();
    assert!(opening.await.unwrap_err().is_cancelled());

    let mut shutdown = Box::pin(catalog.shutdown(deadline));
    let during_handshake = futures_util::poll!(shutdown.as_mut());
    drop(socket);
    assert!(
        during_handshake.is_pending(),
        "shutdown returned {during_handshake:?} during PostgreSQL startup"
    );
    shutdown.await.unwrap();
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
        format!(
            "{dsn}{separator}options=-c%20application_name%3D{name}%20-c%20jit%3Don%20-c%20work_mem%3D11MB%20-c%20statement_timeout%3D250ms"
        )
    } else {
        format!(
            "{dsn} options='-c application_name={name} -c jit=on -c work_mem=11MB -c statement_timeout=250ms'"
        )
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
    assert_eq!(pool.connection_status().size, 4);
    assert_eq!(
        pool.batch_slots_available(),
        Some(default_settings().batch_concurrency)
    );
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
                "SELECT current_setting('jit'), current_setting('application_name'),
                        current_setting('work_mem'), current_setting('statement_timeout')",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(row.get::<_, String>(0), "off");
        assert_eq!(row.get::<_, String>(1), name);
        assert_eq!(row.get::<_, String>(2), "11MB");
        assert_eq!(row.get::<_, String>(3), "250ms");
    }
    drop(deadpool_postgres::Object::take(held.pop().unwrap()));
    let replacement = pool.get().await.unwrap();
    let row = replacement
        .query_one(
            "SELECT current_setting('jit'), current_setting('work_mem'), current_setting('statement_timeout')",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, &str>(0), "off");
    assert_eq!(row.get::<_, &str>(1), "11MB");
    assert_eq!(row.get::<_, &str>(2), "250ms");
    let error = replacement
        .query_one("SELECT pg_sleep(1)", &[])
        .await
        .unwrap_err();
    assert_eq!(
        error.code(),
        Some(&tokio_postgres::error::SqlState::QUERY_CANCELED)
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
async fn configured_batch_budgets_are_shared_by_clones_and_isolated_between_pools() {
    use maptile_core::sources::{
        TileCoord, TileSource,
        postgres::{PostgresFunctionConfig, PostgresFunctionSource},
    };
    use std::{sync::Arc, time::Duration};
    let Some(dsn) = common::postgres::pg_dsn() else {
        return;
    };
    let schema = common::postgres::SchemaGuard::create(&dsn).await;
    let (monitor, connection) = tokio_postgres::connect(&dsn, tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    monitor
        .batch_execute(&format!(
            r#"CREATE TABLE "{s}".gate(id integer);
            INSERT INTO "{s}".gate VALUES(1);
            CREATE FUNCTION "{s}".budget_batch(z integer,x integer,y integer)
            RETURNS bytea LANGUAGE plpgsql AS $$ BEGIN
              PERFORM 1 FROM "{s}".gate FOR SHARE;
              RETURN decode('1a0c0a05726f6164732880207802','hex');
            END $$;"#,
            s = schema.name
        ))
        .await
        .unwrap();
    let mut groups = Vec::new();
    let mut names = Vec::new();
    for limit in [1, 3] {
        let name = format!("{}_budget_{limit}", schema.name);
        let separator = if dsn.starts_with("postgres://") || dsn.starts_with("postgresql://") {
            if dsn.contains('?') { "&" } else { "?" }
        } else {
            " "
        };
        let pool = PostgresPool::connect(
            &format!("{dsn}{separator}application_name={name}"),
            PoolSettings {
                batch_concurrency: limit,
                ..default_settings()
            },
        )
        .await
        .unwrap();
        let config = PostgresFunctionConfig {
            id: name.clone(),
            schema: schema.name.clone(),
            function: "budget_batch".into(),
            description: None,
            minzoom: None,
            maxzoom: None,
            bounds: None,
            cache_zoom: None,
        };
        let original = Arc::new(
            PostgresFunctionSource::open(Arc::new(pool.clone()), config.clone())
                .await
                .unwrap(),
        );
        let cloned = Arc::new(
            PostgresFunctionSource::open(Arc::new(pool), config)
                .await
                .unwrap(),
        );
        groups.push((limit, [original, cloned]));
        names.push(name);
    }
    monitor
        .batch_execute(&format!(
            "BEGIN; LOCK TABLE \"{}\".gate IN EXCLUSIVE MODE",
            schema.name
        ))
        .await
        .unwrap();
    let mut jobs = Vec::new();
    for (limit, sources) in groups {
        for index in 0..=limit {
            let source = sources[index % 2].clone();
            jobs.push(tokio::spawn(async move {
                source
                    .read_tile_batch(&[TileCoord { z: 0, x: 0, y: 0 }], None)
                    .await
            }));
        }
    }
    let counts = "SELECT count(*) FILTER (WHERE application_name=$1),
                         count(*) FILTER (WHERE application_name=$2)
                  FROM pg_stat_activity WHERE wait_event_type='Lock'";
    let saturated = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            // The lock holder's transaction must refresh its statistics snapshot.
            monitor
                .batch_execute("SELECT pg_stat_clear_snapshot()")
                .await
                .unwrap();
            let row = monitor
                .query_one(counts, &[&names[0], &names[1]])
                .await
                .unwrap();
            if (row.get::<_, i64>(0), row.get::<_, i64>(1)) == (1, 3) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    let excess = tokio::time::timeout(Duration::from_millis(150), async {
        loop {
            monitor
                .batch_execute("SELECT pg_stat_clear_snapshot()")
                .await
                .unwrap();
            let row = monitor
                .query_one(counts, &[&names[0], &names[1]])
                .await
                .unwrap();
            if row.get::<_, i64>(0) > 1 || row.get::<_, i64>(1) > 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    monitor.batch_execute("ROLLBACK").await.unwrap();
    for job in jobs {
        tokio::time::timeout(Duration::from_secs(3), job)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
    assert!(
        saturated.is_ok(),
        "independent configured budgets did not fill"
    );
    assert!(excess.is_err(), "pool clones exceeded their shared budget");
}

#[tokio::test]
async fn batch_sql_admission_survives_a_controlled_cancellation_storm() {
    let _batch_test = BATCH_TEST_LOCK.lock().await;
    use maptile_core::{
        cache::TileCache,
        control::CatalogService,
        service::{ReadTarget, TileStack},
        sources::{
            TileCoord, TileQuery,
            postgres::{
                PostgresFunctionConfig, PostgresFunctionSource, PostgresTableConfig,
                PostgresTableSource,
            },
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
          IF (SELECT enabled FROM "{s}".control) THEN
            BEGIN
              PERFORM nextval('{s}.started');
              PERFORM pg_sleep(10);
            EXCEPTION WHEN query_canceled THEN
              PERFORM pg_sleep(0.15);
            END;
          END IF;
          RETURN value;
        END $$;
        CREATE TABLE "{s}".point(id bigint,name text,geom geometry(Point,4326));
        INSERT INTO "{s}".point VALUES(1,'point',ST_SetSRID(ST_Point(0,0),4326));
        CREATE VIEW "{s}".storm_batch AS SELECT id,"{s}".slow_name(name) AS name,geom FROM "{s}".point;
        CREATE FUNCTION "{s}".storm_function(z integer,x integer,y integer) RETURNS bytea LANGUAGE plpgsql VOLATILE AS $$
        BEGIN PERFORM "{s}".slow_name('point'); RETURN decode('1a0c0a05726f6164732880207802','hex'); END $$;
    "#,s=schema.name)).await.unwrap();
    let pool = Arc::new(
        PostgresPool::connect(&named_dsn, default_settings())
            .await
            .unwrap(),
    );
    let mut config = PostgresTableConfig::new("storm-table", &schema.name, "storm_batch");
    config.bounds = Some([-180., -85., 180., 85.]);
    config.id_column = Some("id".into());
    config.properties = Some(vec!["name".into()]);
    let table = PostgresTableSource::open(pool.clone(), config)
        .await
        .unwrap()
        .with_allowed_columns(Arc::new(std::collections::HashSet::from(["id".into()])));
    let function = PostgresFunctionSource::open(
        pool,
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
    .unwrap();
    let engine = Arc::new(CatalogService::new(TileCache::new(1, None, None).unwrap()));
    let mut update = engine.catalog().update();
    update.put(Arc::new(table)).unwrap();
    update.put(Arc::new(function)).unwrap();
    engine.catalog().publish(update).unwrap();
    let stack = TileStack::builder(engine).build();

    monitor
        .query_one(
            &format!("SELECT setval('{}.started',1,false)", schema.name),
            &[],
        )
        .await
        .unwrap();
    let coords = [TileCoord {
        z: 14,
        x: 8192,
        y: 8192,
    }];
    let spawn = |source: &'static str, filtered: bool| {
        let stack = stack.clone();
        tokio::spawn(async move {
            let expr = filtered.then(|| maptile_core::filter::parse_cql2("id=1").unwrap());
            let query = expr.as_ref().map(|expr| TileQuery {
                filter: Some(expr),
                ..TileQuery::default()
            });
            let mut target = ReadTarget::new(source, "public");
            target.query = query.as_ref();
            stack.read_batch(target, &coords).await
        })
    };
    let mut current = spawn("storm-table", false);
    let persistent = spawn("storm-function", false);
    let active_sql =
        "SELECT count(*) FROM pg_stat_activity WHERE application_name=$1 AND state='active'";
    let start_sql = format!("SELECT last_value FROM \"{}\".started", schema.name);
    let activity_sql = format!("SELECT ({active_sql}), ({start_sql})");
    let mut peak = 0_i64;
    let mut starved = false;
    let mut expected = 2_i64;
    let variants = [
        ("storm-table", false),
        ("storm-table", true),
        ("storm-function", false),
    ];
    for replacement in 0..=8 {
        if replacement > 0 {
            current.abort();
            let _ = current.await;
            expected = monitor
                .query_one(&start_sql, &[])
                .await
                .unwrap()
                .get::<_, i64>(0)
                + 1;
            let (source, filtered) = variants[(replacement - 1) % variants.len()];
            current = spawn(source, filtered);
        }
        let started = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let row = monitor.query_one(&activity_sql, &[&name]).await.unwrap();
                peak = peak.max(row.get::<_, i64>(0));
                if row.get::<_, i64>(1) >= expected {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await;
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
    let stopped = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let row = monitor.query_one(active_sql, &[&name]).await.unwrap();
            let active = row.get::<_, i64>(0);
            peak = peak.max(active);
            if active == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await;
    let restored = tokio::time::timeout(
        Duration::from_secs(2),
        futures_util::future::try_join(
            stack.read_batch(ReadTarget::new("storm-table", "public"), &coords),
            stack.read_batch(ReadTarget::new("storm-function", "public"), &coords),
        ),
    )
    .await;
    monitor.query("SELECT pg_cancel_backend(pid) FROM pg_stat_activity WHERE application_name=$1 AND state='active'",&[&name]).await.unwrap();
    println!(
        "SQL_ADMISSION peak={peak} replacements=8 starved={starved} stopped={} restored={}",
        stopped.is_ok(),
        restored.as_ref().is_ok_and(|r| r.is_ok())
    );
    assert_eq!(peak, 2, "unexpected actual PostgreSQL batch concurrency");
    assert!(!starved, "a replacement batch never acquired its SQL slot");
    assert!(stopped.is_ok(), "cancelled SQL did not finish cleanup");
    let (table, function) = restored
        .expect("SQL admission slots were not restored")
        .unwrap();
    for page in [table, function] {
        assert_eq!(page.consumed, 1);
        assert!(
            page.tiles[0]
                .tile()
                .is_some_and(|tile| !tile.data.is_empty())
        );
    }
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
            BEGIN
              -- The test can cancel either backend as soon as both signals are visible.
              PERFORM nextval('{s}.started');
              PERFORM pg_sleep(30);
            EXCEPTION WHEN query_canceled THEN
              PERFORM pg_sleep(12);
            END;
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

#[tokio::test]
async fn batch_transport_loss_stops_only_the_affected_pool() {
    let Some(dsn) = common::postgres::pg_dsn() else {
        return;
    };
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
    let independent_batch = tokio::time::timeout(
        Duration::from_secs(1),
        online_source.read_tile_batch(&[TileCoord { z: 1, x: 1, y: 0 }], None),
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
    assert!(
        independent_batch.unwrap().is_ok(),
        "a failed pool closed an independent pool's batch admission"
    );
}

#[test]
fn tls_initialization_without_global_provider() {
    const CHILD: &str = "MAPTILE_TEST_PROVIDER_CHILD";
    if let Ok(mode) = std::env::var(CHILD) {
        assert!(rustls::crypto::CryptoProvider::get_default().is_none());
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let error = runtime
            .block_on(PostgresPool::connect(
                &format!("host=127.0.0.1 port=0 user=test dbname=test sslmode={mode}"),
                default_settings(),
            ))
            .unwrap_err();
        assert!(
            matches!(error, maptile_core::error::MaptileError::Backend(_)),
            "{error:?}"
        );
        assert!(rustls::crypto::CryptoProvider::get_default().is_none());
        return;
    }
    for mode in ["disable", "prefer", "require"] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tls_initialization_without_global_provider",
                "--nocapture",
            ])
            .env(CHILD, mode)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{mode}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// Dedicated fixtures: TLS and plaintext DSNs omit sslmode; the CA is trusted
/// only inside child processes, without mutating the test runner's environment.
#[test]
fn tls_modes_negotiate_and_verify_certificates() {
    const CHILD: &str = "MAPTILE_TEST_TLS_CASE";
    if let Ok(case) = std::env::var(CHILD) {
        assert!(rustls::crypto::CryptoProvider::get_default().is_none());
        let dsn = std::env::var("MAPTILE_TEST_CASE_DSN").unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let result = PostgresPool::connect(&dsn, default_settings()).await;
            if matches!(
                case.as_str(),
                "untrusted_prefer"
                    | "untrusted_require"
                    | "wrong_hostname"
                    | "wrong_hostname_prefer"
                    | "plain_require"
            ) {
                let error = result.unwrap_err();
                let detail = format!("{error:?}");
                if case == "plain_require" {
                    assert!(detail.contains("server does not support TLS"), "{detail}");
                } else {
                    let reason = if case.starts_with("wrong_hostname") {
                        "NotValidForName"
                    } else {
                        "UnknownIssuer"
                    };
                    assert!(
                        detail.contains("InvalidCertificate") && detail.contains(reason),
                        "{detail}"
                    );
                }
                println!("{case}: rejected {detail}");
            } else {
                let pool = result.unwrap();
                let conn = pool.get().await.unwrap();
                let ssl: bool = conn
                    .query_one(
                        "SELECT ssl FROM pg_stat_ssl WHERE pid=pg_backend_pid()",
                        &[],
                    )
                    .await
                    .unwrap()
                    .get(0);
                let expected = matches!(case.as_str(), "prefer" | "require" | "default");
                assert_eq!(ssl, expected, "{case}");
                println!("{case}: pg_stat_ssl.ssl={ssl}");
            }
        });
        assert!(rustls::crypto::CryptoProvider::get_default().is_none());
        return;
    }
    let Ok(tls_dsn) = std::env::var("MAPTILE_TEST_TLS_DSN") else {
        eprintln!("MAPTILE_TEST_TLS_DSN unset — skipping TLS fixture test");
        return;
    };
    let plain_dsn = std::env::var("MAPTILE_TEST_PLAIN_DSN").expect("plaintext fixture DSN");
    let socket_dsn = std::env::var("MAPTILE_TEST_SOCKET_DSN").expect("Unix socket fixture DSN");
    let ca = std::env::var("MAPTILE_TEST_CA_FILE").expect("test CA PEM");
    let config = tls_dsn.parse::<tokio_postgres::Config>().unwrap();
    let port = config.get_ports().first().copied().unwrap_or(5432);
    let wrong_hostname_dsn = format!(
        "host=wrong.invalid hostaddr=127.0.0.1 port={port} user=test dbname=test sslmode=require"
    );
    let empty = tempfile::tempdir().unwrap();
    let empty_ca = empty.path().join("empty.pem");
    std::fs::write(&empty_ca, "").unwrap();
    for (case, dsn, trusted) in [
        ("default", tls_dsn.clone(), true),
        ("disable", format!("{tls_dsn} sslmode=disable"), true),
        ("prefer", format!("{tls_dsn} sslmode=prefer"), true),
        ("require", format!("{tls_dsn} sslmode=require"), true),
        (
            "untrusted_prefer",
            format!("{tls_dsn} sslmode=prefer"),
            false,
        ),
        (
            "untrusted_require",
            format!("{tls_dsn} sslmode=require"),
            false,
        ),
        (
            "wrong_hostname_prefer",
            wrong_hostname_dsn.replace("sslmode=require", "sslmode=prefer"),
            true,
        ),
        ("wrong_hostname", wrong_hostname_dsn, true),
        ("plain_prefer", format!("{plain_dsn} sslmode=prefer"), true),
        (
            "plain_require",
            format!("{plain_dsn} sslmode=require"),
            true,
        ),
        (
            "socket_prefer",
            format!("{socket_dsn} sslmode=prefer"),
            true,
        ),
        (
            "socket_disable",
            format!("{socket_dsn} sslmode=disable"),
            true,
        ),
    ] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tls_modes_negotiate_and_verify_certificates",
                "--nocapture",
            ])
            .env(CHILD, case)
            .env("MAPTILE_TEST_CASE_DSN", dsn)
            .env(
                "SSL_CERT_FILE",
                if trusted {
                    std::path::Path::new(&ca)
                } else {
                    &empty_ca
                },
            )
            .env("SSL_CERT_DIR", empty.path())
            .output()
            .unwrap();
        println!("{}", String::from_utf8_lossy(&output.stdout));
        assert!(
            output.status.success(),
            "{case}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn tls_batch_cancellation_keeps_the_pools_initial_trust() {
    const CHILD: &str = "MAPTILE_TEST_CANCEL_CA_CHANGE";
    if let Ok(change) = std::env::var(CHILD) {
        use maptile_core::sources::postgres::{PostgresFunctionConfig, PostgresFunctionSource};
        use maptile_core::sources::{TileCoord, TileSource};
        use std::{sync::Arc, time::Duration};
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let admin_dsn = common::postgres::pg_dsn().expect("real PostgreSQL fixture");
            let schema = common::postgres::SchemaGuard::create(&admin_dsn).await;
            let (monitor, connection) = tokio_postgres::connect(&admin_dsn, tokio_postgres::NoTls).await.unwrap();
            tokio::spawn(async move { connection.await.unwrap(); });
            monitor.batch_execute(&format!(
                "CREATE FUNCTION {}.cancel_ca(z integer, x integer, y integer) RETURNS bytea LANGUAGE plpgsql AS $$ BEGIN IF x = 0 THEN PERFORM pg_sleep(8); END IF; RETURN decode('1a0c0a05726f6164732880207802','hex'); END $$;",
                schema.name)).await.unwrap();
            let dsn = format!("{} application_name={}", std::env::var("MAPTILE_TEST_CASE_DSN").unwrap(), schema.name);
            let pool = PostgresPool::connect(&dsn, PoolSettings { batch_concurrency: 1, ..default_settings() }).await.unwrap();
            let conn = pool.get().await.unwrap();
            assert!(conn.query_one("SELECT ssl FROM pg_stat_ssl WHERE pid=pg_backend_pid()", &[]).await.unwrap().get::<_, bool>(0));
            drop(conn);
            // Clone must keep exactly the same trust as the pool's connection manager.
            let cloned = Arc::new(pool.clone());
            let source = Arc::new(PostgresFunctionSource::open(cloned.clone(), PostgresFunctionConfig {
                id: "cancel-ca".into(), schema: schema.name.clone(), function: "cancel_ca".into(),
                description: None, minzoom: None, maxzoom: None, bounds: None, cache_zoom: None,
            }).await.unwrap());
            let job = { let source = source.clone(); tokio::spawn(async move {
                source.read_tile_batch(&[TileCoord { z: 1, x: 0, y: 0 }], None).await
            }) };
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    let active: bool = monitor.query_one("SELECT EXISTS(SELECT FROM pg_stat_activity WHERE application_name=$1 AND state='active' AND wait_event='PgSleep')", &[&schema.name]).await.unwrap().get(0);
                    if active { break; }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }).await.unwrap();
            let ca = std::env::var("SSL_CERT_FILE").unwrap();
            match change.as_str() {
                "remove" => std::fs::remove_file(&ca).unwrap(),
                "replace" => std::fs::write(&ca, b"not a certificate").unwrap(),
                "unchanged" => (),
                _ => panic!("unknown change"),
            }
            let started = std::time::Instant::now();
            job.abort(); assert!(job.await.unwrap_err().is_cancelled());
            let stopped = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let active: bool = monitor.query_one("SELECT EXISTS(SELECT FROM pg_stat_activity WHERE application_name=$1 AND state='active')", &[&schema.name]).await.unwrap().get(0);
                    if !active { break; }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }).await;
            let elapsed = started.elapsed();
            // Clean up our own backend even when the regression is present.
            if stopped.is_err() {
                monitor.query("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE application_name=$1", &[&schema.name]).await.unwrap();
            }
            assert!(stopped.is_ok(), "{change}: cancellation failed after {elapsed:?}");
            tokio::time::timeout(Duration::from_secs(2), source.read_tile_batch(&[TileCoord { z: 1, x: 1, y: 0 }], None)).await.unwrap().unwrap();
            // The now-replaced backend also reuses the initial trust snapshot.
            let conn = cloned.get().await.unwrap();
            assert!(conn.query_one("SELECT ssl FROM pg_stat_ssl WHERE pid=pg_backend_pid()", &[]).await.unwrap().get::<_, bool>(0));
            println!("{change}: cancellation={elapsed:?}; next batch and TLS replacement succeeded");
        });
        return;
    }
    let Ok(dsn) = std::env::var("MAPTILE_TEST_TLS_DSN") else {
        eprintln!("MAPTILE_TEST_TLS_DSN unset — skipping TLS cancellation fixture");
        return;
    };
    let ca = std::env::var("MAPTILE_TEST_CA_FILE").expect("test CA PEM");
    for mode in ["prefer", "require"] {
        for change in ["unchanged", "remove", "replace"] {
            let dir = tempfile::tempdir().unwrap();
            let local_ca = dir.path().join("ca.pem");
            std::fs::copy(&ca, &local_ca).unwrap();
            let empty = dir.path().join("empty");
            std::fs::create_dir(&empty).unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tls_batch_cancellation_keeps_the_pools_initial_trust",
                    "--nocapture",
                ])
                .env(CHILD, change)
                .env("MAPTILE_TEST_CASE_DSN", format!("{dsn} sslmode={mode}"))
                .env("SSL_CERT_FILE", &local_ca)
                .env("SSL_CERT_DIR", &empty)
                .output()
                .unwrap();
            println!("{mode}: {}", String::from_utf8_lossy(&output.stdout));
            assert!(
                output.status.success(),
                "{mode}/{change}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}
