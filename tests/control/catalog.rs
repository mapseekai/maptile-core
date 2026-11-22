use std::time::Duration;

use tokio::time::Instant;

use super::{DiscoveredDataset, Inventory, SourceCatalog};
use crate::{cache::ContentVersion, error::MaptileError, transport::CATALOG_BUILD_LEASE};

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
            CATALOG_BUILD_LEASE.with(|lease| lease.mark_unconfirmed());
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
