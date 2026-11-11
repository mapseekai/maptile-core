//! Style catalog — JSON-friendly style metadata for `/styles.json`-style endpoints.
//!
//! The HTTP layer (axum / volo / ...) serialises [`StyleInfo`] directly; the
//! core just builds the structure.

use crate::error::MaptileResult;
use crate::styles::store::StyleStore;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StyleInfo {
    pub id: String,
    pub name: String,
    /// Public URL to `style.json`. `None` when the catalog is built without a
    /// `base_url` (e.g. internal admin views).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// Snapshot every style id known to `store` and produce a list of
/// [`StyleInfo`]s. When `base_url` is `Some`, the `url` field is populated
/// using the standard `<base>/styles/<id>/style.json[<query_string>]` pattern.
pub async fn snapshot(
    store: &dyn StyleStore,
    base_url: Option<&str>,
    query_string: &str,
) -> MaptileResult<Vec<StyleInfo>> {
    let ids = store.list().await?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let style = store.get(&id).await?;
        let url = base_url.map(|b| {
            let b = b.trim_end_matches('/');
            format!("{b}/styles/{id}/style.json{query_string}")
        });
        out.push(StyleInfo {
            id: style.id,
            name: style.name,
            url,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::styles::store::LocalStyleStore;
    use std::fs;
    use std::path::PathBuf;

    fn write(p: &std::path::Path, b: &[u8]) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, b).unwrap();
    }

    #[tokio::test]
    async fn snapshot_with_base_url_populates_urls() {
        let tmp = tempfile::tempdir().unwrap();
        let bright = tmp.path().join("bright/style.json");
        let dark = tmp.path().join("dark/style.json");
        write(&bright, br#"{"version":8,"name":"Bright"}"#);
        write(&dark, br#"{"version":8,"name":"Dark"}"#);
        let store =
            LocalStyleStore::new::<_, &str>(vec![("bright", bright), ("dark", dark)]).unwrap();
        let infos = snapshot(&store, Some("https://map.example/"), "?key=k")
            .await
            .unwrap();
        assert_eq!(infos.len(), 2);
        let by_id: std::collections::HashMap<_, _> =
            infos.iter().map(|i| (i.id.clone(), i)).collect();
        assert_eq!(
            by_id["bright"].url.as_deref(),
            Some("https://map.example/styles/bright/style.json?key=k")
        );
        assert_eq!(by_id["bright"].name, "Bright");
    }

    #[tokio::test]
    async fn snapshot_without_base_url_omits_urls() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("s/style.json");
        write(&p, br#"{"version":8}"#);
        let store = LocalStyleStore::new::<_, &str>(vec![("s", p)]).unwrap();
        let infos = snapshot(&store, None, "").await.unwrap();
        assert!(infos[0].url.is_none());
    }

    // Compile-time sanity: signature accepts any &dyn StyleStore.
    #[allow(dead_code)]
    fn _signature_check(
        store: &dyn StyleStore,
    ) -> impl std::future::Future<Output = MaptileResult<Vec<StyleInfo>>> + '_ {
        snapshot(store, None, "")
    }

    #[allow(dead_code)]
    fn _path_buf_unused(_p: PathBuf) {}
}
