# maptile-core

`maptile-core` 是用于构建地图瓦片服务与静态地图应用的 Rust 组件库。它通过统一的数据源接口，提供瓦片归档读取、PostGIS 动态矢量切片、GeoParquet 与本地矢量数据处理、COG 栅格切片、缓存、样式管理和 MapLibre Native 渲染能力。应用可以按需启用 Cargo features，将这些组件接入自己的服务、桌面工具或数据处理流程。

## 核心能力

| 使用场景 | 提供的能力 | 接入入口 |
| --- | --- | --- |
| 发布已有瓦片 | 读取本地、HTTP 和对象存储中的 PMTiles，以及本地 MBTiles；获取瓦片与 TileJSON 元数据 | [数据源与查询](docs/sources.md) |
| 从数据库生成矢量瓦片 | PostGIS 表动态生成 MVT，调用 PostgreSQL 瓦片函数，按属性与空间范围查询 | [PostgreSQL 接入](docs/sources.md#postgresql-与-postgis) |
| 从矢量文件生成瓦片 | GeoParquet 动态切片、FlatGeobuf 直接切片、GeoJSON 与 ZIP Shapefile 导入 | [GeoParquet 与本地矢量](docs/geoparquet.md) |
| 发布遥感与栅格数据 | COG 动态切片、波段与色带处理、原始像元查询、STAC 检索与镶嵌 | [样式与渲染](docs/rendering.md) |
| 生成地图图片 | MapLibre Native 渲染、静态地图、标记与路径叠加、PNG/JPEG/WebP 输出 | [地图图片渲染](docs/rendering.md#从样式生成地图图片) |
| 组织服务运行 | 数据源注册与替换、MVT 组合、字节计权缓存、ETag、文件发现和缓存指标 | [架构与生命周期](docs/architecture.md) |

默认构建包含 PMTiles、MBTiles、PostgreSQL、MVT 组合、CQL2 属性过滤、缓存、样式管理和压缩工具。GeoParquet、栅格处理、原生渲染等能力通过 [功能开关](docs/features.md) 选择。

## 获取代码

```sh
git clone https://github.com/mapseekai/maptile-core.git
cd maptile-core
```

仓库使用 [rust-toolchain.toml](rust-toolchain.toml) 固定 Rust 1.98.1，并通过 [Cargo.lock](Cargo.lock) 固定依赖解析结果。构建前准备 Rust 工具链和 C/C++ 编译工具；各可选后端的环境要求见[开发与构建](docs/development.md)。

```sh
rustup show active-toolchain
cargo --version
```

## 快速开始

仓库提供一个可直接运行的[PMTiles 读取示例](examples/read_pmtiles.rs)：

```sh
cargo run --locked --example read_pmtiles
```

示例打开仓库内的合成 PMTiles 归档，注册数据源，通过共享缓存读取 `0/0/0`，然后关闭数据源。预期输出：

```text
sample 0/0/0: 14 bytes
```

这个合成载荷用于验证文件读取与缓存接入。使用业务地图时，将路径和坐标替换为自己归档中的瓦片。完整步骤见[读取第一张瓦片](docs/getting-started.md)。

### 在 Rust 项目中接入

通过 Git 依赖或本地 path 依赖使用本库：

```toml
[dependencies]
maptile-core = { git = "https://github.com/mapseekai/maptile-core.git" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

在交付项目中，可用 `rev` 固定经过验证的 Git 提交。以下示例读取调用方准备的 PMTiles 文件：

```rust
use maptile_core::prelude::*;
use maptile_core::sources::pmtiles::{LocalPmtilesSource, PmtilesConfig};
use std::{sync::Arc, time::Duration};

#[tokio::main]
async fn main() -> MaptileResult<()> {
    let cache = Arc::new(TileCache::new(
        64,
        Some(Duration::from_secs(3600)),
        None,
    )?);
    let manager = SourceManager::new().with_cache(cache);
    let source = LocalPmtilesSource::open(PmtilesConfig {
        id: "basemap".into(),
        path: "data/basemap.pmtiles".into(),
        serve_as: None,
        description: None,
    })
    .await?;

    manager.register(Arc::new(source));
    let tile = manager
        .get_tile("basemap", TileCoord { z: 0, x: 0, y: 0 }, None)
        .await?;
    println!("{} bytes, {:?}", tile.data.len(), tile.info);
    manager.close_source("basemap").await?;
    Ok(())
}
```

`TileResponse` 同时返回瓦片字节和本次响应的格式、压缩编码。应用据此组织响应、存储文件或继续处理数据。

## 文档导航

从[文档首页](docs/README.md)按任务选择阅读路径：

- [读取第一张瓦片](docs/getting-started.md)：运行示例、接入自己的文件、处理响应。
- [功能开关](docs/features.md)：选择 Cargo features 及其依赖组合。
- [数据源与查询](docs/sources.md)：归档、数据库、过滤、组合与数据更新。
- [GeoParquet 与本地矢量](docs/geoparquet.md)：本地与远程数据、动态切片、格式导入和资源预算。
- [样式与渲染](docs/rendering.md)：地图样式、栅格样式、图片渲染和 WMTS 元数据。
- [架构与生命周期](docs/architecture.md)：模块职责、缓存身份、数据源关闭和应用集成。
- [开发与构建](docs/development.md)：环境准备、测试、Rustdoc 和原生渲染构建。

## 构建与参与

常规检查入口如下，更多组合与外部测试环境见[开发与构建](docs/development.md)：

```sh
cargo fmt --all --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo doc --locked --no-deps
```

通过 [GitHub Issues](https://github.com/mapseekai/maptile-core/issues) 提交问题或功能建议。提交 PR 时，请说明使用场景、启用的 features、复现步骤和验证结果；涉及公共 API 的修改应同步示例与文档。开发约定见 [AGENTS.md](AGENTS.md)。

## 许可与来源

项目自有代码采用 [MIT 许可证](LICENSE)。部分实现参考或改编自 Martin 与 tileserver-rs；版权归属、许可文本和原生渲染分发要求见[第三方声明](docs/third-party-notices.md)。

本项目从 MapSeek 的瓦片核心库提取，来源记录见[独立项目来源](docs/EXTRACTION.md)，版本迁移信息见[历史变更记录](docs/CHANGELOG.md)。
