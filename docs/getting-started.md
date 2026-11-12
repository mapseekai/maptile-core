# 读取第一张瓦片

本教程通过仓库自带的 PMTiles 合成样例，验证 `LocalPmtilesSource`、`SourceManager` 和 `TileCache` 的接入流程。完成后，你可以用同样的 API 读取自己的瓦片归档。

实现与完整代码见 [`examples/read_pmtiles.rs`](../examples/read_pmtiles.rs)，数据说明见[测试样例](../tests/fixtures/README.md)。

## 1. 准备项目

准备 Rust 1.98.1、Git 和本机 C/C++ 编译工具，然后获取仓库：

```sh
git clone https://github.com/mapseekai/maptile-core.git
cd maptile-core
rustup show active-toolchain
```

仓库中的 `rust-toolchain.toml` 会让 rustup 选择指定版本。额外后端的环境准备见[开发与构建](development.md)。

## 2. 运行读取示例

在仓库根目录运行：

```sh
cargo run --locked --example read_pmtiles
```

程序读取 `tests/fixtures/sample.pmtiles` 的 `z=0、x=0、y=0`，预期输出：

```text
sample 0/0/0: 14 bytes
```

这 14 字节是合成载荷 `test-tile-data`，用于验证归档读取和组件连接。示例使用 `CARGO_MANIFEST_DIR` 定位文件，因此路径由仓库位置确定。

## 3. 理解调用过程

示例依次完成以下操作：

1. 用 `TileCache::new(64, Some(Duration::from_secs(3600)), None)?` 创建共享缓存。容量参数按 MiB 换算，第二个参数配置存活时间，第三个参数配置空闲过期时间。
2. 用 `SourceManager::new().with_cache(cache)` 创建带缓存的数据源管理器。
3. 用 `LocalPmtilesSource::open(config).await?` 打开归档，并将源注册到管理器。
4. 用 `get_tile("sample", TileCoord { z: 0, x: 0, y: 0 }, None)` 读取瓦片。
5. 用 `close_source("sample").await?` 移除数据源、失效其缓存并等待源自身的关闭操作。

缓存处理与关闭语义详见[架构与生命周期](architecture.md)。

## 4. 接入自己的 PMTiles

在应用的 `Cargo.toml` 中声明依赖：

```toml
[dependencies]
maptile-core = { git = "https://github.com/mapseekai/maptile-core.git" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

本地联调可以将 Git 依赖改为 `maptile-core = { path = "../maptile-core" }`。从[项目首页的完整示例](../README.md#在-rust-项目中接入)开始，将 `path` 设置为自己的归档，使用归档内存在的瓦片坐标。

`PmtilesConfig.id` 是应用内的数据源标识，后续查询和替换操作使用同一个 ID。`serve_as: None` 使用归档声明的格式；格式转换可通过[数据源与查询](sources.md#格式与压缩编码)中介绍的转码入口完成。

注册前可通过 `source.tilejson()` 查看边界、层级和图层信息；注册后可调用 `manager.get(id)` 取得源，或用 `snapshot_tilejson()` 获取目录快照。`TileSource` trait 由 `prelude` 导出。

## 5. 处理瓦片结果

`get_tile` 返回 [`TileResponse`](../src/sources/mod.rs)，其字段说明如下：

| 字段 | 内容 | 使用方式 |
| --- | --- | --- |
| `data` | 本次瓦片的 `Bytes` 载荷 | 返回给客户端、写入文件，或交给解码器 |
| `info.format` | 本次响应的瓦片格式 | 选择 MVT、图片或其他格式的处理路径 |
| `info.encoding` | 本次载荷的压缩编码 | 设置响应编码或调用 `transport::decode` |
| `last_modified` | 数据源提供的可选修改时间 | 构建应用层的缓存验证信息 |

常用结果类型通过 `prelude` 导入。下面的完整函数按本次响应的编码解压瓦片载荷：

```rust
use maptile_core::prelude::*;

fn decoded_bytes(tile: &TileResponse) -> MaptileResult<Bytes> {
    maptile_core::transport::decode(&tile.data, tile.info.encoding)
}
```

应用需要 ETag 验证时，可使用 `manager.get_tile_with_etag(...)`，并将 `TileEtagResponse::Fresh` 或 `NotModified` 映射为相应的应用响应。具体签名见 [`SourceManager`](../src/sources/manager.rs)。

## 后续任务

- 接入 MBTiles、PostGIS、CQL2 或组合图层：阅读[数据源与查询](sources.md)。
- 从 GeoParquet、FlatGeobuf、GeoJSON 或 Shapefile 生成 MVT：阅读[GeoParquet 与本地矢量](geoparquet.md)。
- 生成静态地图和栅格图片：阅读[样式与渲染](rendering.md)。
- 选择依赖组合并运行检查：阅读[功能开关](features.md)和[开发与构建](development.md)。
