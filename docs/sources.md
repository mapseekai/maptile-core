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

MBTiles 的相对路径在打开时固定为绝对别名路径，后续改变进程当前目录不影响源或其克隆。主库、WAL 和 symlink 别名仍进行文件身份检查；替换文件、切换 symlink 目标或提交新的 WAL 后，旧源拒绝读取，需要重新打开并注册。

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

`properties` 选择输出属性，`with_allowed_columns` 指定 CQL2 可以引用的字段。两者分别控制输出内容与查询条件。几何列、ID 列和属性配置应对应实际表结构，空间索引用于支撑范围查询。普通、批量和过滤读取都由 SQL 判断空间相交，保留 MVT buffer（64/4096）内的边界要素；普通与批量读取仍保留由数据范围推导的 minzoom 短路。

### 属性与空间范围查询

[`filter`](../src/filter.rs)接收 CQL2 文本或 JSON，支持比较、`AND`、`OR`、`NOT`、`IN`、`LIKE` 和空值判断。PostGIS 表源依据已配置的字段白名单验证条件。空间范围通过 `TileQuery.bbox` 单独传入，坐标顺序为 WGS84 的 `[west, south, east, north]`。

PostgreSQL 表源、函数源、OutDB 源及独立 DuckDB 源在单张和批量入口检查 `z <= 30`、`x,y < 2^z`，以及 typed `bbox` 的四个值均为有限数；非法输入返回 `MaptileError::InvalidRequest`，不会执行 SQL。合法坐标处于源自身 zoom 范围外时仍沿用原来的空瓦片或 `TileNotFound` 行为。

`bbox` 保持现有合约，不新增经纬度范围或端点顺序限制。表源使用 `geom && ST_MakeEnvelope(w,s,e,n,4326)` 的平面包围盒语义；例如 `[170,-10,-170,10]` 仍表示两端点之间的范围，不会自动拆成日期线两侧。函数源将有限的 bbox 按原顺序作为逗号分隔字符串交给四参数函数。DuckDB 和 OutDB 当前不使用 `TileQuery.bbox` 筛选数据；入口校验不改变这一能力边界。

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

### TLS 连接

锁定的 PostgreSQL 驱动支持 `sslmode=disable`、`prefer`（默认）和 `require`，不支持 `verify-ca` 或 `verify-full` 名称：

- `disable` 使用未加密连接。
- `prefer` 通过 TCP 首先尝试 TLS，服务器明确不支持 TLS 时回退；证书校验失败不会降级。
- `require` 要求 TLS，验证可信证书链及连接主机名。

TLS 使用显式 ring provider，无需宿主安装全局 rustls provider，也不修改宿主的全局 provider。`prefer` 的 TLS 会话使用与 `require` 相同的证书验证；根证书由 `rustls-native-certs` 从系统信任库或进程的 `SSL_CERT_FILE` / `SSL_CERT_DIR` 加载。私有 CA 可通过这些环境变量在进程启动前提供 PEM 文件。启用 TLS 的自签服务需要配置可信 CA，不能依赖 `prefer` 绕过校验。Unix socket 可使用 `disable` 或 `prefer`，不提供 TLS 加密。

根证书在创建连接池时加载一次；连接池的克隆、补建连接和批量查询取消均复用同一信任配置。之后删除或替换 CA 文件不会阻止已建立池的取消请求，也不会动态改变该池的信任集合；应用更新 CA 后应重新创建连接池。

### 配置连接池

`PoolSettings` 的默认配置如下，单位为毫秒的字段在名称中带有 `_ms`：

| 字段 | 默认值 | 用途 |
| --- | --- | --- |
| `max_size` | `16` | 连接池容量 |
| `batch_concurrency` | `2` | 每个池的批量查询额度，包含取消后的清理任务 |
| `wait_timeout_ms` | `5000` | 等待空闲连接的期限 |
| `create_timeout_ms` | `5000` | 创建连接的期限 |
| `recycle_timeout_ms` | `5000` | 回收检查的期限 |
| `pre_warm` | `false` | 启动时按需建连；设为 `true` 时并发预建最多 `max_size` 条连接 |

连接初始化会设置 `jit=off`，并保留 DSN 中其他启动选项；后续扩容和替换连接沿用同一配置。开启预建连接时，根据数据库总连接预算配置池容量，并将这部分建连成本计入启动时间。

`batch_concurrency` 必须为正数且不超过 Tokio 信号量上限。克隆 `PostgresPool` 会共享同一额度；分别 `connect` 得到的池具有独立额度。在线 `get_tile` 不占批量额度，但与批量查询共享连接池。如果 SQL 取消后无法确认后端已经结束，该池停止接受新批量任务；核实数据库残留任务后再重建池。应用还可在启动时通过 `transport::configure_blocking_concurrency` 配置本 crate 的共享阻塞任务额度，见[并发额度](architecture.md#并发额度)。

Postgres 表源和函数源不持有瓦片结果缓存。原先的源级 `with_cache`，以及函数源的 `set_cache` / `cache` 接口已移除；将缓存配置迁移到 `SourceManager::with_cache`，并通过管理器读取。`cache_zoom` 继续控制管理器缓存的层级，直接调用源会重新查询数据库。

```rust
use maptile_core::sources::postgres::PoolSettings;

let settings = PoolSettings {
    max_size: 12,
    batch_concurrency: 3,
    ..PoolSettings::default()
};
```

表源打开和刷新时发现元数据并生成查询模板，SQL 预编译发生在实际使用的连接上。源码依据见 [`pool.rs`](../src/sources/postgres/pool.rs) 和 [`table.rs`](../src/sources/postgres/table.rs)。

## 固定栅格镶嵌与成员范围

`MosaicSource` 用固定的 `MosaicMember` 列表生成栅格瓦片。每个 `bbox_wgs84` 都是该成员自己的 WGS84 `[west, south, east, north]` 范围，与配置的 `srid` 无关；`MosaicSourceConfig::bounds` 和 `srid` 只控制对外公布的合并范围，不覆盖成员范围。请求先将合法瓦片范围转换为 WGS84，按正面积相交筛选成员并保留原顺序，再由上游应用默认每瓦片 64 个候选的限制；只有边界相接的成员不计入候选。WebMercatorQuad 和 WorldCrs84Quad 的首列西界、末列东界分别采用精确的 -180° 和 +180°，避免世界外缘成员被浮点舍入误排；内列边界仍使用 TMS 计算值。正面积相交不保证输出有有效像素；极窄影像仍受上游投影、读取和栅格采样精度限制。

显式 `bounds` 的四个分量必须有限且南纬不大于北纬，否则打开源时返回 `MaptileError::Config`。未指定或非正数 `srid` 沿用 WGS84，纬度必须在 `[-90, 90]` 内，包含两极和等边点范围。正数非 4326 的 SRID 先按原有 GDAL 路径投影，再检查 WGS84 输出域，不把投影坐标当作纬度。展开经度仍可输入，例如 `190..200` 对外公布为 `-170..-160`；跨日期线或覆盖整圈时，单个 TileJSON bbox 用 `[-180, 180]` 包住两侧。这样 bounds 符合 [TileJSON 3.0 的坐标域](https://github.com/mapbox/tilejson-spec/blob/master/3.0.0/README.md#35-bounds)，center 位于公布的范围内。`MosaicSource` 本身仅在 `raster` feature 下导出，`stac` 单独启用不开放此 API。

跨日期线可以用 `west > east` 表示，也可提供递增的未折回经度，例如 `175..185`。成员范围未知时可用 `[0; 4]`；非有限、倒置纬度、越出纬度范围或退化 bbox 都保守地作为所有瓦片的候选，并计入上限。因此 65 个未知范围仍可能超限，应尽量提供准确的成员范围。接近 360° 的跨度如果在折回时舍入为同一经度，会保守覆盖整条纬度带，避免漏选成员。未显式配置合并范围时，未知成员会扩大 TileJSON 范围到世界，跨日期线成员会使单个 TileJSON bbox 覆盖两侧。

全局统计仍使用完整、有序的成员集，不能用单瓦片候选替代。锁定的 async-geotiff 0.13.0 在统计发现阶段也应用默认 64 个资产的限制，超过 64 个成员的全局统计拉伸仍不受支持；需要统计的样式还会读取远处成员，远处坏文件可能使统计失败。普通固定样式下，65 个成员中只有 1 个相交的场景已由真实投影 TIFF 回归覆盖。

内容版本包含成员身份和归一化范围，修改范围后重新打开并注册会分开瓦片缓存。成员内容本身变化时，应用仍需更新其 `source_id`/内容身份。配置与真实样式示例见[栅格渲染](rendering.md#stac固定镶嵌与-outdb)及 [`mosaic_source.rs`](../src/sources/mosaic_source.rs)。

## FlatGeobuf 文件版本

FlatGeobuf 在 Unix 上将完整 FileStamp（含设备号、inode、ctime、长度和 mtime）纳入内容版本，同长度、同 mtime 的不同 inode 替换文件重新打开后也获得不同版本。其他平台每次打开分配新代次，克隆共享版本。更新文件后仍须重新打开和注册；热缓存命中不会自动执行文件变化检查。查询与关闭流程见[矢量文件指南](geoparquet.md#直接读取-flatgeobuf)。

## 组合多个 MVT 源

`CompositeSource` 合并多个 MVT 源的图层。已有源注册后，可以通过管理器创建组合源：

```rust
use maptile_core::prelude::*;

fn register_city_layers(manager: &SourceManager) -> MaptileResult<()> {
    manager.register_composite_from_ids("city", &["roads", "buildings"])
}
```

这里的 `roads` 和 `buildings` 应预先注册并输出 MVT。查询 `city` 时，组合源按成员配置读取并合并瓦片；`CompositeChild` 可配置图层名前缀。需要在读取时按 ID 解析当前成员的应用，可使用 `LazyCompositeSource`。实现见 [`composite`](../src/sources/composite.rs) 和 [`lazy_composite`](../src/sources/lazy_composite.rs)。

`LazyCompositeSource` 跳过缺失成员；全部成员缺失时，单瓦片读取返回 `TileNotFound`，批量读取保留对应的空瓦片。没有可用成员且存在加载失败记录时返回 `SourceUnavailable`。成员格式错误或管理器已销毁等错误会继续向调用方传播，批量导出也不会将它们当作成功的空结果。

普通、ETag 和批量读取都会检查组合源的实例解析路径：间接循环或超过 64 层的惰性组合返回 `Config`，避免递归耗尽栈。检查发生在读取时，因此成员晚注册或替换形成的环同样会被拒绝。合法嵌套以及多个分支共享一个子组仍可使用；静态组合保留的旧实例与同 ID 新实例不会被误判为循环。直接自引用仍按原约定跳过。

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

异步故障回调应携带实际使用的源引用，调用 `manager.mark_unavailable_if_current(&source)`；按 ID 的 `mark_unavailable` 用于管理操作。请求触发恢复时，直接调用 `recover_unavailable`，由管理器检查退避、协调工厂并校验结果身份：

```rust,no_run
use maptile_core::prelude::*;
use maptile_core::sources::mbtiles::{MbtilesConfig, MbtilesSource};
use std::sync::Arc;

async fn recover_basemap(manager: &SourceManager) -> MaptileResult<Arc<dyn TileSource>> {
    manager.recover_unavailable("basemap", || async {
        let source = MbtilesSource::open(MbtilesConfig {
            id: "basemap".into(),
            path: "data/basemap.mbtiles".into(),
            serve_as: None,
            description: None,
        }).await?;
        Ok(Arc::new(source) as Arc<dyn TileSource>)
    }).await
}
```

退避期间或同一注册项已有恢复任务时返回 `SourceUnavailable`，未获准的调用不执行工厂。若源已经就绪，直接返回当前引用；若已删除，返回 `SourceNotFound`。不要把 `unavailable_retry_due` 的观察结果当成独占恢复资格。
