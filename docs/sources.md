# 数据源与查询

`maptile-core` 通过 [`TileSource`](../src/sources/mod.rs) 统一瓦片读取与元数据访问。应用打开具体数据源，将其注册到 [`SourceManager`](../src/sources/manager.rs)，再按源 ID 和 XYZ 坐标请求瓦片。本文说明归档、数据库、过滤和组合源的接入方式。

## 按数据形态选择入口

| 数据 | 入口 | Feature | 准备条件 |
| --- | --- | --- | --- |
| 本地 PMTiles v3 | `LocalPmtilesSource::open` | 默认 | 可读取的归档路径 |
| HTTP(S) PMTiles v3 | `HttpPmtilesSource::open` | 默认 | 提供 Range 和 206 响应的文件地址 |
| 对象存储 PMTiles | `CloudPmtilesSource::open_with_store` | `cloud` | 应用注入 `ObjectStore` 客户端 |
| 本地 MBTiles | `MbtilesSource::open` | 默认 | 可读取的 SQLite 归档 |
| PostGIS 表 | `PostgresTableSource::open` | 默认 | PostgreSQL 连接池、空间表与几何列 |
| PostgreSQL 瓦片函数 | `PostgresFunctionSource::open` | 默认 | 返回 MVT `bytea` 的 SQL 函数 |
| GeoParquet / FlatGeobuf | `GeoParquetSource` / `FlatGeobufSource` | `geoparquet` / `local-vector` | [矢量文件指南](geoparquet.md)中的扩展与数据配置 |
| DuckDB 查询 | `DuckDbSource::open` | `duckdb` | DuckDB 文件或连接配置、单条 SQL 模板 |
| COG / STAC / 栅格镶嵌 | `AsyncCogSource` / `StacSource` / `MosaicSource` | `agtiff` / `stac` / `raster` | [栅格接入](rendering.md)中的样式和读取环境 |

各源的构造参数见对应 Rustdoc。Feature 的依赖关系见[功能开关](features.md)。

## PMTiles 与 MBTiles

PMTiles 的本地、HTTP 和对象存储入口共享 `PmtilesConfig`。`id` 用于注册，`path` 指定文件或地址，`description` 可补充目录说明。完整本地示例见[快速开始](getting-started.md)。

MBTiles 使用 `MbtilesConfig`，底层读取器负责 SQLite 连接池、元数据和 TMS/XYZ 行号转换。调用方统一使用 `TileCoord { z, x, y }`：

```rust
use maptile_core::prelude::*;
use maptile_core::sources::mbtiles::{MbtilesConfig, MbtilesSource};
use std::sync::Arc;

async fn register_mbtiles(manager: &SourceManager) -> MaptileResult<()> {
    let source = MbtilesSource::open(MbtilesConfig {
        id: "basemap".into(),
        path: "data/basemap.mbtiles".into(),
        serve_as: None,
        description: Some("基础地图".into()),
    })
    .await?;
    manager.register(Arc::new(source));
    Ok(())
}
```

归档发布时，保持服务中的文件快照稳定。准备好新版本文件后，重新打开并注册，具体步骤见[数据更新与关闭](#数据更新与关闭)。

PMTiles 读取器为目录与元数据设置 4 MiB 的读取、解压预算，单张瓦片归档载荷预算为 16 MiB。依据见 [`pmtiles/reader.rs`](../src/sources/pmtiles/reader.rs)；解码预算见 [`transport`](../src/transport.rs)。

归档制作和维护可使用重导出的 `PmTilesWriter`、`PmTilesStreamWriter`，以及 `sources::mbtiles::tools` 中的复制、补丁、验证、元数据和层级更新 API。

## PostgreSQL 与 PostGIS

### 打开空间表

[`PostgresPool`](../src/sources/postgres/pool.rs)建立连接时检查数据库版本，最低版本常量为 PostgreSQL 11 和 PostGIS 3.0。表源读取几何列、SRID、属性和边界等元数据，按瓦片范围生成 MVT。版本常量与导出类型见 [`postgres`](../src/sources/postgres/mod.rs)。

下面的函数接入一个包含 `geom`、`id`、`name` 和 `class` 列的 `public.roads` 表。应用将自己的 DSN 传入函数：

```rust
use maptile_core::prelude::*;
use maptile_core::sources::postgres::{
    PoolSettings, PostgresPool, PostgresTableConfig, PostgresTableSource,
};
use std::{collections::HashSet, sync::Arc};

async fn open_roads(dsn: &str) -> MaptileResult<SourceManager> {
    let pool = Arc::new(PostgresPool::connect(dsn, PoolSettings::default()).await?);
    let mut config = PostgresTableConfig::new("roads", "public", "roads");
    config.geometry_column = Some("geom".into());
    config.id_column = Some("id".into());
    config.properties = Some(vec!["name".into(), "class".into()]);

    let allowed: HashSet<String> = ["name", "class"]
        .into_iter()
        .map(String::from)
        .collect();
    let source = PostgresTableSource::open(pool, config)
        .await?
        .with_allowed_columns(Arc::new(allowed));

    let manager = SourceManager::new();
    manager.register(Arc::new(source));
    Ok(manager)
}
```

`properties` 选择输出属性，`with_allowed_columns` 指定 CQL2 可以引用的字段。两者分别控制输出内容与查询条件。几何列、ID 列和属性配置应对应实际表结构，空间索引用于支撑范围查询。

### 属性与空间范围查询

[`filter`](../src/filter.rs)接收 CQL2 文本或 JSON，支持比较、`AND`、`OR`、`NOT`、`IN`、`LIKE` 和空值判断。PostGIS 表源依据已配置的字段白名单验证条件。空间范围通过 `TileQuery.bbox` 单独传入，坐标顺序为 WGS84 的 `[west, south, east, north]`。

在上面的 `roads` 源注册完成后，可以按道路类别读取瓦片：

```rust
use maptile_core::filter::parse_cql2;
use maptile_core::prelude::*;

async fn read_primary_roads(manager: &SourceManager) -> MaptileResult<TileResponse> {
    let filter = parse_cql2("class = 'primary'")?;
    let query = TileQuery {
        filter: Some(&filter),
        bbox: Some([115.4, 39.4, 117.6, 41.1]),
        ..TileQuery::default()
    };
    manager
        .get_tile("roads", TileCoord { z: 10, x: 843, y: 388 }, Some(&query))
        .await
}
```

此类查询走请求专用 SQL 路径。普通瓦片查询在各数据库连接上通过 `prepare_typed_cached` 准备并复用 SQL；过滤查询使用按请求准备的语句。两种路径都应依据返回值的 `TileResponse.info` 处理编码。

### 调用瓦片函数

`PostgresFunctionSource` 支持以下函数签名，返回类型为 MVT `bytea`：

- `function(z integer, x integer, y integer)`。
- `function(z integer, x integer, y integer, query json)`。
- `function(z integer, x integer, y integer, query jsonb)`。

四参数形式将 URL 查询参数和 CQL2 JSON 交给数据库函数。应用与函数共同约定这些参数的业务含义。连接和参数探测实现见 [`postgres/function.rs`](../src/sources/postgres/function.rs)。

### 配置连接池

`PoolSettings` 的默认配置如下，单位为毫秒的字段在名称中带有 `_ms`：

| 字段 | 默认值 | 用途 |
| --- | --- | --- |
| `max_size` | `16` | 连接池容量 |
| `wait_timeout_ms` | `5000` | 等待空闲连接的期限 |
| `create_timeout_ms` | `5000` | 创建连接的期限 |
| `recycle_timeout_ms` | `5000` | 回收检查的期限 |
| `pre_warm` | `false` | 启动时按需建连；设为 `true` 时并发预建最多 `max_size` 条连接 |

连接初始化会设置 `jit=off`，并保留 DSN 中其他启动选项；后续扩容和替换连接沿用同一配置。开启预建连接时，根据数据库总连接预算配置池容量，并将这部分建连成本计入启动时间。

表源打开和刷新时发现元数据并生成查询模板，SQL 预编译发生在实际使用的连接上。源码依据见 [`pool.rs`](../src/sources/postgres/pool.rs) 和 [`table.rs`](../src/sources/postgres/table.rs)。

## 组合多个 MVT 源

`CompositeSource` 合并多个 MVT 源的图层。已有源注册后，可以通过管理器创建组合源：

```rust
use maptile_core::prelude::*;

fn register_city_layers(manager: &SourceManager) -> MaptileResult<()> {
    manager.register_composite_from_ids("city", &["roads", "buildings"])
}
```

这里的 `roads` 和 `buildings` 应预先注册并输出 MVT。查询 `city` 时，组合源按成员配置读取并合并瓦片；`CompositeChild` 可配置图层名前缀。需要在读取时按 ID 解析当前成员的应用，可使用 `LazyCompositeSource`。实现见 [`composite`](../src/sources/composite.rs) 和 [`lazy_composite`](../src/sources/lazy_composite.rs)。

## 格式与压缩编码

`TileResponse.info.format` 描述数据格式，`info.encoding` 描述本次响应字节的压缩方式。将两者与载荷一起传递，可以让客户端、转码器和组合源使用正确的解码路径。

[`transport`](../src/transport.rs)提供显式 gzip、zlib、brotli、zstd 编解码，以及异步包装。启用 `mlt` 后，[`transcode`](../src/transcode/mod.rs)提供 MVT 与 MLT 转换。`transcode_with_gzip` 可自动识别 gzip、zstd、zlib；Brotli 载荷先通过已知编码调用 `transport::decode`，再传入转码器。

独立 DuckDB 源可通过单条 SQL 模板返回预编码 BLOB，或配置 WGS84 WKB 几何列生成 MVT。其连接等待和执行共享 10 秒期限，配置与模板语法见 [`duckdb.rs`](../src/sources/duckdb.rs)。GeoParquet 的查询预算单独通过 `query_options` 配置。

## 数据更新与关闭

应用更新数据时，按以下顺序维护数据源与缓存的一致性：

1. 准备具有明确版本身份的新数据，例如带版本的文件路径、对象存储 URI 或数据库修订号。
2. 打开并检查新源，使用目标数据源 ID 调用 `register`。管理器会将后续查询切换到新对象，并对旧对象发起 `start_close`。
3. 对相同内容版本内的业务更新，按应用策略调用 `TileCache::invalidate_source`；PostGIS 表源也可通过 `with_edit_revision` 将业务修订号加入内容身份。
4. 下线一个源时，调用 `close_source(id).await`，完成移除、缓存失效和资源关闭等待。

目录扫描与 `reload_from_discovery` 用于同步新增和删除的源 ID。更新已有 ID 的文件时，使用重新打开和注册流程。缓存键、内容版本和显式关闭的关系见[架构与生命周期](architecture.md)。
