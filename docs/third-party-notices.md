# 第三方声明

本项目自有代码采用根目录 [MIT 许可证](../LICENSE)。第三方代码与依赖保留原始版权和许可证，本页不取代它们的许可文本。

## Martin

- PostgreSQL 表源的瓦片查询模板参考了 Martin 的 `table_to_query`。 [本项目实现](../src/sources/postgres/table.rs) · [上游实现](https://github.com/maplibre/martin/blob/main/martin/src/config/file/tiles/postgres/resolver/query_tables.rs)
- 复合 MVT 瓦片源参考了 Martin 的多源组合方式。 [本项目实现](../src/sources/composite.rs)
- 缓存指标参考了 `martin-core` 的指标结构。 [本项目实现](../src/metrics.rs)

Martin 采用 MIT 或 Apache-2.0 双许可。 [上游仓库](https://github.com/maplibre/martin) · [MIT 版权与许可](../third_party/martin/LICENSE-MIT) · [Apache-2.0 许可](../third_party/martin/LICENSE-APACHE)

## tileserver-rs

下列文件包含来自 [tileserver-rs](https://github.com/vinayakkulkarni/tileserver-rs) 的移植或改编代码：

- `src/render/overlay.rs`：源自 `crates/tileserver-rs/src/render/overlay.rs`。
- `src/transcode/mod.rs` 和 `src/transcode/mvt_mlt.rs`：源自 `crates/tileserver-rs/src/transcode.rs`。

PostgreSQL 表源的瓦片查询模板也参考了 tileserver-rs 的 `build_tile_query`。文件发现和瓦片缓存响应头参考了其对应实现。 [瓦片查询](../src/sources/postgres/table.rs) · [文件发现](../src/discovery.rs) · [缓存响应头](../src/headers.rs)

完整 MIT 版权和许可声明保留于 [third_party/tileserver-rs/LICENSE](../third_party/tileserver-rs/LICENSE)，分发这些文件或其衍生物时须保留。2026-11-11 以本机上游检出 `c74cd433e3427d7209ceef96669c713f262e48e0` 核对许可文本；该提交不是已证实的最初移植版本。

## MapLibre Native

`render` 使用 [MapLibre Native](https://github.com/maplibre/maplibre-native)，许可证为 BSD-2-Clause。分发包含该渲染器的产物时，必须保留适用版权及许可声明。

发布包还必须附带源码构建时由 CMake 生成的 `mbgl-core.license`，包括适用的 Boost 等依赖声明。本页不能替代构建产物随附的完整许可证集合。

## Cargo 依赖

Cargo.toml 和 Cargo.lock 记录依赖及解析版本，各组件保留自己的版权与许可证。具体分发形式仍需要审查实际包含的直接和传递依赖；本页不是完整法律审查或二进制许可清单。
