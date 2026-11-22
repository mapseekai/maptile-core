# 读取第一张瓦片

本教程通过仓库自带的 PMTiles 合成样例，验证 `LocalPmtilesSource`、`CatalogService` 和 `TileStack` 的接入流程。完成后，你可以用同样的 API 读取自己的瓦片归档。

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
cargo run --locked --features pmtiles --example read_pmtiles
```

程序读取 `tests/fixtures/sample.pmtiles` 的 `z=0、x=0、y=0`，预期输出：

```text
sample 0/0/0: 14 bytes (Mvt, Uncompressed)
```

这 14 字节是合成载荷 `test-tile-data`，用于验证归档读取和组件连接。示例使用 `CARGO_MANIFEST_DIR` 定位文件，因此路径由仓库位置确定。

## 3. 理解调用过程

示例依次完成以下操作：

1. 用 `TileCache::new` 和 `CatalogService::new` 创建带缓存的目录服务。
2. 用 `LocalPmtilesSource::open(config).await?` 打开归档。
3. 用 `update.put` 和 `catalog.publish` 原子发布目录。
4. 用 `TileStack::builder(catalog).build()` 构建默认栈。
5. 用 `stack.read(ReadTarget::new("sample", "cli").at(coord)).await` 读取瓦片。

`scope` 用于隔离规范缓存。应用先鉴权，再根据实际可见范围设定 scope。目录快照和关闭语义详见[架构与生命周期](architecture.md)。

## 4. 接入自己的 PMTiles

在应用的 `Cargo.toml` 中声明依赖：

```toml
[dependencies]
maptile-core = { git = "https://github.com/mapseekai/maptile-core.git", features = ["pmtiles"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

本地联调可以将 Git 依赖改为 `maptile-core = { path = "../maptile-core" }`。从[项目首页的完整示例](../README.md#在-rust-项目中接入)开始，将 `path` 设置为自己的归档，使用归档内存在的瓦片坐标。

`PmtilesConfig.id` 是应用内的数据源标识，后续查询和替换操作使用同一个 ID。`serve_as: None` 使用归档声明的格式；格式转换可通过[数据源与查询](sources.md#格式与压缩编码)中介绍的转码入口完成。

注册前可通过 `source.tilejson()` 查看元数据；发布后可用 `catalog.catalog().snapshot().describe(id)` 查看同一代目录中的描述符。内部 TileJSON 不含对外瓦片地址。

## 5. 处理瓦片结果

`stack.read` 返回 `TileOutcome`。`NoTile` 表示没有瓦片，`Present` 包含完整的 `TileResponse`，`Partial` 表示组合源降级，可带瓦片或没有瓦片。`TileResponse` 的字段如下：

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

HTTP 示例根据完整瓦片的最终格式、编码和响应字节计算强 ETag，并处理 `If-None-Match`。部分结果使用 `Cache-Control: no-store`。 [HTTP 示例](../examples/archive_http.rs)

在仓库根目录启动示例，再访问 `http://127.0.0.1:3000/tilejson.json` 或 `/wmts.xml`：

```sh
cargo run --locked --features pmtiles --example archive_http -- http://127.0.0.1:3000
```

参数是应用配置的可信公开 origin。示例只监听 `127.0.0.1:3000`，使用与 CLI 相同的目录和归档。也可以在 origin 后传入其他 PMTiles 路径。在应用中接入时，按服务配置补充鉴权，并使 WMTS 声明与瓦片输出一致。 [WMTS 元数据示例](rendering.md#生成-wmts-元数据)

## 后续任务

- 接入 MBTiles、PostGIS、CQL2 或组合图层：阅读[数据源与查询](sources.md)。
- 从 GeoParquet 或 FlatGeobuf 生成 MVT：阅读[GeoParquet 与本地矢量](geoparquet.md)。
- 生成静态地图和栅格图片：阅读[样式与渲染](rendering.md)。
- 选择依赖组合并运行检查：阅读[功能开关](features.md)和[开发与构建](development.md)。
