# 第三方声明

本项目自有代码采用根目录 [MIT 许可证](../LICENSE)。第三方代码和依赖保留各自的版权与许可，适用授权和分发要求以对应的原始许可证及构建产物随附声明为准。

## Martin

以下实现参考了 Martin：

| 本项目内容 | 参考内容 | 实现入口 |
| --- | --- | --- |
| PostgreSQL 表源查询模板 | Martin 的 `table_to_query` | [本项目](../src/sources/postgres/table.rs) · [上游](https://github.com/maplibre/martin/blob/main/martin/src/config/file/tiles/postgres/resolver/query_tables.rs) |
| 复合 MVT 瓦片源 | 多源组合方式 | [composite.rs](../src/sources/composite.rs) |
| 缓存指标 | `martin-core` 指标结构 | [metrics.rs](../src/metrics.rs) |

[Martin](https://github.com/maplibre/martin)采用 MIT 或 Apache-2.0 双许可。仓库保留完整的 [MIT 版权与许可](../third_party/martin/LICENSE-MIT)和 [Apache-2.0 许可](../third_party/martin/LICENSE-APACHE)。

## tileserver-rs

以下文件包含来自 [tileserver-rs](https://github.com/vinayakkulkarni/tileserver-rs) 的移植或改编代码：

| 本项目文件 | 上游来源 |
| --- | --- |
| [src/render/overlay.rs](../src/render/overlay.rs) | `crates/tileserver-rs/src/render/overlay.rs` |
| [src/transcode/mod.rs](../src/transcode/mod.rs)、[src/transcode/mvt_mlt.rs](../src/transcode/mvt_mlt.rs) | `crates/tileserver-rs/src/transcode.rs` |

[PostgreSQL 表源](../src/sources/postgres/table.rs)的瓦片查询模板参考了 tileserver-rs 的 `build_tile_query`。[文件发现](../src/discovery.rs)和[缓存响应头](../src/headers.rs)参考了其对应实现。

完整 MIT 版权和许可声明保留于 [third_party/tileserver-rs/LICENSE](../third_party/tileserver-rs/LICENSE)，分发这些文件或其衍生物时须保留。2026-11-11 以本机上游检出 `c74cd433e3427d7209ceef96669c713f262e48e0` 核对许可文本；该提交用于本次许可核对，最初移植版本待考证。

## MapLibre Native

`render` 使用 [MapLibre Native](https://github.com/maplibre/maplibre-native)，许可证为 BSD-2-Clause。分发包含该渲染器的产物时，保留适用的版权与许可声明。

发布包还须附带源码构建时由 CMake 生成的 `mbgl-core.license`，其中包含适用的 Boost 等依赖声明。原生构建流程见[开发与构建](development.md#原生渲染构建)。

## Cargo 依赖

[Cargo.toml](../Cargo.toml)和 [Cargo.lock](../Cargo.lock)记录依赖与解析版本，各组件保留自己的版权和许可证。生成分发清单时，应按实际产物包含的直接及传递依赖，核对完整的许可文本和随附声明。
