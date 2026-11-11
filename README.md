# maptile-core

`maptile-core` 提供瓦片源、缓存、过滤、样式和渲染组件，由调用方注入配置及运行环境。库本身不承担 HTTP 路由、身份验证或配置文件解析。它是独立 Cargo 项目，不需要 MapSeek 服务或工作区。

## 最小使用示例

当前 [Cargo.toml](Cargo.toml)声明版本 0.3.0、Rust 1.98.1、edition 2024。尚未发布到 crates.io，可从 Git 仓库或本地 path 引用：

```toml
[dependencies]
maptile-core = { git = "https://github.com/mapseekai/maptile-core.git" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

生产项目应通过 `rev` 固定经过验证的提交。使用 GeoParquet 时，在 maptile-core 依赖声明上添加 `features = ["geoparquet"]`。以下代码需要准备实际 PMTiles 文件：

```rust
use maptile_core::prelude::*;
use maptile_core::sources::pmtiles::{LocalPmtilesSource, PmtilesConfig};
use std::{sync::Arc, time::Duration};

#[tokio::main]
async fn main() -> MaptileResult<()> {
    let cache = TileCache::new(512, Some(Duration::from_secs(3600)), None)?;
    let manager = SourceManager::new().with_cache(Arc::new(cache));
    let source = LocalPmtilesSource::open(PmtilesConfig {
        id: "planet".into(),
        path: "data/planet.pmtiles".into(),
        serve_as: None,
        description: None,
    }).await?;
    manager.register(Arc::new(source));
    let tile = manager.get_tile("planet", TileCoord { z: 4, x: 8, y: 5 }, None).await?;
    println!("{} bytes", tile.data.len());
    Ok(())
}
```

`TileCache::new` 返回 Result，容量换算可能失败，必须处理错误。缓存按字节计权，可设置 TTL/TTI，通过 `get_or_insert` 合并并发 miss；`invalidate_source` 按源失效。

## 功能开关

默认 feature 列表为空。以当前 manifest 为准选择，而不是一次启用所有原生后端。

| Feature | 能力及依赖 |
| --- | --- |
| 默认 | PMTiles、MBTiles、PostgreSQL、组合源、CQL2、样式、压缩与发现 |
| `cloud` | object_store 云端 PMTiles |
| `agtiff` | async-geotiff COG 动态切片，无 GDAL，但需要原生 PROJ |
| `raster` | GDAL 栅格/OutDB，并启用 agtiff |
| `stac` | STAC 发现与 Raster Style 镶嵌，启用 agtiff |
| `geoparquet` | Parquet/Arrow 归档及 DuckDB Spatial 按需几何瓦片，支持 HTTP/S3 范围读取，需预装匹配的 Spatial/HTTPFS 扩展 |
| `local-vector` | 包含 `geoparquet`；本地 FlatGeobuf 直接生成 MVT，GeoJSON / ZIP Shapefile 一次导入为 GeoParquet |
| `duckdb` | 内嵌 DuckDB 查询及配置几何列的动态瓦片 |
| `mlt` | MVT 与 MLT 转换 |
| `metrics` | Prometheus 缓存计数器 |
| `render` | 固定版本 `maplibre_native =0.9.3` 的渲染池 |
| `render-opengl` | render 的 OpenGL 构建变体 |
| `render-bench` | 原生基准专用生命周期测量 |

## 模块导航

| 模块 | 主要接口或用途 |
| --- | --- |
| `sources` | `SourceManager`、PMTiles/MBTiles/Postgres、Composite、COG/STAC |
| `cache` | `TileCache`、`TileCacheKey`、`CacheZoomRange` |
| `filter` | CQL2 AST 与受限 SQL 生成 |
| `styles` | 样式读取及原生 URL 重写 |
| `render` | 线程绑定的原生池、静态地图、叠加与 WMTS |
| `raster` | 波段运算、色带、像元选择与编解码 |
| `wkb_mvt`、`transcode` | WKB→MVT 与 MVT/MLT 转换 |
| `discovery`、`watch` | 文件扫描、源变更发现与重新注册 |
| `transport` | gzip/zlib/brotli/zstd |
| `prelude` | 常用类型和瓦片数学函数的统一导出 |

字体和精灵图由调用方提供，库接受外部 glyph/sprite URL；不包含 MapSeek Resource 服务。

## PostgreSQL 连接与预编译

数据连接在建立时设置 `jit=off`，保留 DSN 中其他启动选项；扩容和替换连接也使用相同设置。`PoolSettings.pre_warm=true` 仅在创建连接池时并发预建最多 `max_size` 条连接，保留各连接直到这轮结束；失败记录警告。关闭时按请求需要创建连接。预建连接会增加启动成本和空闲连接占用，应按数据库的总连接预算配置。

打开或刷新表源只做必要的元数据发现，不借走全池连接、不排队执行全池 SQL 预热。瓦片查询沿用 `prepare_typed_cached`，每个连接首次使用某条 SQL 时准备，之后复用；代价是冷连接的首次查询有一次准备开销。空闲连接优先复用最近使用的一条，以保留其SQL缓存热度；请求等待仍由连接池的信号量管理，读取不经过预热锁。

真实数据库回归位于 `tests/sources/postgres/pool.rs` 和 `table.rs`。可手动比较冷启动至首瓦片，以及已有地图访问期间注册数据源的延迟：

```sh
DATABASE_URL=... cargo test -p maptile-core --test postgres_table \
  registration_under_tile_load -- --ignored --nocapture
```

该微基准使用16连接、12次并发打开同一小表、8路共1600次不经瓦片结果缓存的请求；先单列冷启动成本，再将连接用于热查询对比。它用于观察连接竞争，不代表大表、生产容量或发布构建性能。

## 构建和检查

Rust 工具链固定为 1.98.1，默认 features 为空。项目有独立锁文件，依赖全部来自 crates.io，没有 MapSeek 内部 path/git 依赖。

```sh
cargo fmt --all --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo doc --locked --no-deps
bash scripts/ci/maptile-render-cargo_test.sh
```

其他能力分别启用：

```sh
cargo test --locked --features cloud,metrics,mlt
cargo run --locked --features geoparquet --example geoparquet_extensions -- --check
cargo test --locked --features geoparquet --lib
cargo test --locked --features local-vector --lib
cargo check --locked --all-targets --features raster,stac
```

GeoParquet 需要预装匹配版本的官方签名 DuckDB 扩展，见[独立使用指南](docs/geoparquet.md)。PostgreSQL 测试需要专用 PostGIS 测试库及 `DATABASE_URL`；未设置时部分测试提前返回，不能视为真实数据库验证。

本地矢量文件入口为 `FlatGeobufSource::open` 和 `import_to_geoparquet`，完整配置及安全边界见[本地矢量文件说明](docs/geoparquet.md#本地-flatgeobuf-与一次性导入)。

### 原生依赖

- `agtiff` / `stac`：不使用 GDAL，但启用了 async-geotiff 的原生 PROJ。当前依赖需要系统 PROJ ≥9.6.2，否则尝试 CMake 源码构建；运行时需要匹配的 PROJ 数据。
- `raster`：还需要 GDAL 及对应开发库。
- `geoparquet` / `duckdb`：编译 bundled DuckDB，需要 C/C++ 工具链；GeoParquet 动态几何另需 Spatial/HTTPFS 扩展。
- `render`：MapLibre Native 0.9.3 绑定及 C/C++、CMake、Ninja。macOS 使用 Metal 与 Xcode Command Line Tools；Linux 默认 Vulkan。
- `render-opengl`：只支持 Linux。macOS 不要用 `--all-features`，应分开验证支持的组合。

可复现的 MapLibre 源码构建脚本保留在本仓库：

```sh
CARGO_TARGET_DIR=/tmp/maptile-core-native-debug \
  bash scripts/ci/maptile-render-cargo.sh test --locked --features render \
  --test render_native -- --test-threads=1
CARGO_TARGET_DIR=/tmp/maptile-core-native-release \
  bash scripts/ci/maptile-render-cargo.sh test --release --locked --features render \
  --test render_native -- --test-threads=1
```

首次验证应使用不存在或空的输出目录。脚本固定上游提交并递归准备子模块，禁用预编译/系统核心及 `DOCS_RS` 跳过编译模式。默认源码缓存位于 `.cache/maplibre-native-src/maplibre-native`。`MAPTILE_MLN_CHECKOUT` 必须以 `/maplibre-native` 结尾；该路径由脚本管理，重跑会重置源码和子模块，不要指向正在开发的检出。

Release 需要核心与 C++ 桥接的 `NDEBUG` 一致，并执行串行原生渲染测试。链接成功不能替代运行验证。分发原生渲染产物时必须附带构建生成的 `mbgl-core.license`，详见[第三方声明](docs/third-party-notices.md)。

## 许可与项目来源

这是从 MapSeek 提取的独立项目。项目自有代码采用 [MIT 许可证](LICENSE)，第三方代码保留各自的版权和许可。

本项目的部分实现参考了 Martin 和 tileserver-rs 的源代码。涉及的模块及上游版权与许可见[第三方声明](docs/third-party-notices.md)。

`publish = false` 防止意外发布到 crates.io；GitHub 公开仓库与 crates.io 发包是独立事项。

- [GeoParquet 独立使用](docs/geoparquet.md)
- [历史变更记录](docs/CHANGELOG.md)
- [提取来源和边界](docs/EXTRACTION.md)
- [第三方声明](docs/third-party-notices.md)
