# 第三方声明

本项目自有代码采用根目录 [MIT 许可证](../LICENSE)。第三方代码与依赖保留原始版权和许可证，本页不取代它们的许可文本。

## tileserver-rs

下列文件包含来自 [tileserver-rs](https://github.com/vinayakkulkarni/tileserver-rs) 的移植或改编代码：

- `src/render/overlay.rs`：源自 `crates/tileserver-rs/src/render/overlay.rs`。
- `src/transcode/mod.rs` 和 `src/transcode/mvt_mlt.rs`：源自 `crates/tileserver-rs/src/transcode.rs`。

完整 MIT 版权和许可声明保留于 [third_party/tileserver-rs/LICENSE](../third_party/tileserver-rs/LICENSE)，分发这些文件或其衍生物时须保留。2026-11-11 以本机上游检出 `c74cd433e3427d7209ceef96669c713f262e48e0` 核对许可文本；该提交不是已证实的最初移植版本。

## MapLibre Native

`render` 使用 [MapLibre Native](https://github.com/maplibre/maplibre-native)，许可证为 BSD-2-Clause。分发包含该渲染器的产物时，必须保留适用版权及许可声明。

发布包还必须附带源码构建时由 CMake 生成的 `mbgl-core.license`，包括适用的 Boost 等依赖声明。本页不能替代构建产物随附的完整许可证集合。

## Cargo 依赖

Cargo.toml 和 Cargo.lock 记录依赖及解析版本，各组件保留自己的版权与许可证。具体分发形式仍需要审查实际包含的直接和传递依赖；本页不是完整法律审查或二进制许可清单。
