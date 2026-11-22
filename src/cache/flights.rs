use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
};

use futures_util::{
    FutureExt,
    future::{BoxFuture, Shared},
};

use super::TileCacheKey;
use crate::{core::response::TileOutcome, error::MaptileResult};

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
    result: Shared<BoxFuture<'static, MaptileResult<TileOutcome>>>,
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
        tile: TileCacheKey,
        generation: u64,
        fill: impl FnOnce() -> BoxFuture<'static, MaptileResult<TileOutcome>>,
    ) -> MaptileResult<TileOutcome> {
        let flight = self.join(FlightKey { tile, generation }, fill);
        flight.result.clone().await
    }

    fn join(
        &self,
        key: FlightKey,
        fill: impl FnOnce() -> BoxFuture<'static, MaptileResult<TileOutcome>>,
    ) -> Arc<Flight> {
        let mut table = self.0.lock().unwrap();
        if let Some(active) = table.get(&key).and_then(Weak::upgrade) {
            return active;
        }
        let flight = Arc::new(Flight {
            table: Arc::downgrade(&self.0),
            key: key.clone(),
            result: fill().shared(),
        });
        table.insert(key, Arc::downgrade(&flight));
        flight
    }
}
