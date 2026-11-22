use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
};

use futures_util::{
    FutureExt,
    future::{BoxFuture, Shared},
};

use tokio::sync::OnceCell;

use super::{TileCache, TileCacheKey};
use crate::{core::response::TileOutcome, error::MaptileResult, sources::TileResponse};

#[derive(Clone, Eq, Hash, PartialEq)]
struct FlightKey {
    tile: TileCacheKey,
    generation: u64,
}
type FlightTable = Mutex<HashMap<FlightKey, Weak<Flight>>>;
#[derive(Default)]
pub(crate) struct Flights(Arc<FlightTable>);
struct Flight {
    table: Weak<FlightTable>,
    key: FlightKey,
    result: Shared<BoxFuture<'static, MaptileResult<FlightResult>>>,
    cached: OnceCell<()>,
}

#[derive(Clone)]
enum FlightResult {
    Cached(TileResponse),
    Loaded(TileOutcome),
}

impl Drop for Flight {
    fn drop(&mut self) {
        if let Some(table) = self.table.upgrade() {
            let mut table = table.lock().unwrap();
            if table
                .get(&self.key)
                .is_some_and(|weak| std::ptr::eq(weak.as_ptr(), self))
            {
                table.remove(&self.key);
            }
        }
    }
}

impl Flights {
    pub(crate) async fn get_or_fill(
        &self,
        cache: &TileCache,
        tile: TileCacheKey,
        generation: u64,
        populate_cache: bool,
        load: impl FnOnce() -> BoxFuture<'static, MaptileResult<TileOutcome>>,
    ) -> MaptileResult<TileOutcome> {
        let key = FlightKey {
            tile: tile.clone(),
            generation,
        };
        let flight = self.join(key, || {
            let cache = cache.clone();
            let tile = tile.clone();
            let load = load();
            async move {
                if let Some(hit) = cache.get_validated(&tile, populate_cache).await? {
                    return Ok(FlightResult::Cached(hit));
                }
                load.await.map(FlightResult::Loaded)
            }
            .boxed()
        });
        match flight.result.clone().await? {
            FlightResult::Cached(hit) => Ok(TileOutcome::Present(hit)),
            FlightResult::Loaded(outcome) => {
                if populate_cache && let TileOutcome::Present(loaded) = &outcome {
                    // Any online waiter can admit the result, including when export started the read.
                    flight
                        .cached
                        .get_or_init(|| cache.insert_validated(tile, loaded.clone()))
                        .await;
                }
                Ok(outcome)
            }
        }
    }

    fn join(
        &self,
        key: FlightKey,
        fill: impl FnOnce() -> BoxFuture<'static, MaptileResult<FlightResult>>,
    ) -> Arc<Flight> {
        let mut table = self.0.lock().unwrap();
        if let Some(active) = table.get(&key).and_then(Weak::upgrade) {
            return active;
        }
        let flight = Arc::new(Flight {
            table: Arc::downgrade(&self.0),
            key: key.clone(),
            result: fill().shared(),
            cached: OnceCell::new(),
        });
        table.insert(key, Arc::downgrade(&flight));
        flight
    }
}
