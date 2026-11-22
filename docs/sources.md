# 数据源与查询

`maptile-core` 通过 [`TileStack`](../src/service/mod.rs) 读取已发布目录中的数据源。应用打开具体源，发布到 [`CatalogService`](../src/service/engine.rs)，再按源 ID 和 XYZ 坐标请求瓦片。已有 `TileSource` 实现可直接加入目录；应用也可实现 `TileDriver`。

## 按数据形态选择入口

| 数据 | 入口 | Feature | 准备条件 |
| --- | --- | --- | --- |
| 本地 PMTiles v3 | `LocalPmtilesSource::open` | `pmtiles` | 可读取的归档路径 |
| HTTP(S) PMTiles v3 | `HttpPmtilesSource::open` | `pmtiles` | 提供 Range 和 206 响应的文件地址 |
| 对象存储 PMTiles | `CloudPmtilesSource::open_with_store` | `pmtiles,cloud` | 应用注入 `ObjectStore` 客户端 |
| 本地 MBTiles | `MbtilesSource::open` | `mbtiles` | 可读取的 SQLite 归档 |
| PostGIS 表 | `PostgresTableSource::open` | `postgres` | PostgreSQL 连接池、空间表与几何列 |
| PostgreSQL 瓦片函数 | `PostgresFunctionSource::open` | `postgres` | 返回 MVT `bytea` 的 SQL 函数 |
| GeoParquet / FlatGeobuf | `GeoParquetSource` / `FlatGeobufSource` | `geoparquet` / `local-vector` | [矢量文件指南](geoparquet.md)中的扩展与数据配置 |
| 预编码 Parquet 瓦片 | `ParquetArchiveSource::open` | `parquet-archive` | 包含坐标和瓦片字节的本地归档 |
| COG / STAC / 栅格镶嵌 | `AsyncCogSource` / `StacSource` / `MosaicSource` | `agtiff` / `stac` / `agtiff` | [栅格接入](rendering.md)中的样式和读取环境 |

各源的构造参数见对应 Rustdoc。Feature 的依赖关系见[功能开关](features.md)。

## PMTiles 与 MBTiles

PMTiles 的本地、HTTP 和对象存储入口共享 `PmtilesConfig`。`id` 用于注册，`path` 指定文件或地址，`description` 可补充目录说明。完整本地示例见[快速开始](getting-started.md)。

MBTiles 使用 `MbtilesConfig`，底层读取器负责 SQLite 连接池、元数据和 TMS/XYZ 行号转换。调用方统一使用 `TileCoord { z, x, y }`：

```rust
use maptile_core::prelude::*;
use maptile_core::sources::mbtiles::{MbtilesConfig, MbtilesSource};
use std::sync::Arc;

async fn register_mbtiles(catalog: &CatalogService) -> MaptileResult<()> {
    let source = MbtilesSource::open(MbtilesConfig {
        id: "basemap".into(),
        path: "data/basemap.mbtiles".into(),
        serve_as: None,
        description: Some("基础地图".into()),
    })
    .await?;
    let mut update = catalog.catalog().update();
    update.put(Arc::new(source))?;
    catalog.catalog().publish(update)?;
    Ok(())
}
```

MBTiles 的相对路径在打开时固定为绝对别名路径，后续改变进程当前目录不影响源或其克隆。每次源端读取在借用连接前、借用连接后及查询后，通过受限的阻塞任务检查主库、WAL 和 symlink 别名。替换文件、切换 symlink 目标或提交新的 WAL 后，旧源拒绝读取，需要重新打开并注册。

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

async fn open_roads(dsn: &str) -> MaptileResult<Arc<CatalogService>> {
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

    let catalog = Arc::new(CatalogService::new(TileCache::new(64, None, None)?));
    let mut update = catalog.catalog().update();
    update.put(Arc::new(source))?;
    catalog.catalog().publish(update)?;
    Ok(catalog)
}
```

`properties` 选择输出属性，`with_allowed_columns` 指定 CQL2 可以引用的字段。空间索引支持带 buffer 的范围查询。表源默认使用 4096 的正数 extent，默认 minzoom 从数据范围推导，也可通过配置覆盖。单取、过滤和批量读取都遵循有效 minzoom。

表源调用 `with_edit_revision(Some(...))` 后可按配置的 zoom 范围缓存。数据发布方需在行数据改变时更新修订并重新发布。无修订的表、函数和 OutDB 直接读取数据库，包含这些源的组合也绕过规范缓存。会话默认 `work_mem=4MB` 和 `statement_timeout=30s`，可通过连接串的 `options` 调整。

### 属性与空间范围查询

[`filter`](../src/filter.rs)接收 CQL2 文本或 JSON，支持比较、`AND`、`OR`、`NOT`、`IN`、`LIKE` 和空值判断。PostGIS 表源依据已配置的字段白名单验证条件。空间范围通过 `TileQuery.bbox` 单独传入，坐标顺序为 WGS84 的 `[west, south, east, north]`。

PostgreSQL 表源、函数源及 OutDB 源在单张和批量入口检查 `z <= 30`、`x,y < 2^z`，以及 typed `bbox` 的四个值均为有限数；非法输入返回 `MaptileError::InvalidRequest`，不会执行 SQL。合法坐标处于源自身 zoom 范围外时仍沿用原来的空瓦片或 `TileNotFound` 行为。

`bbox` 保持现有合约，不新增经纬度范围或端点顺序限制。表源使用 `geom && ST_MakeEnvelope(w,s,e,n,4326)` 的平面包围盒语义；例如 `[170,-10,-170,10]` 仍表示两端点之间的范围，不会自动拆成日期线两侧。函数源将有限的 bbox 按原顺序作为逗号分隔字符串交给四参数函数。OutDB 当前不使用 `TileQuery.bbox` 筛选数据；`TileStack` 对不支持 bbox 的源拒绝该请求。

在上面的 `roads` 源注册完成后，可以按道路类别读取瓦片：

```rust
use maptile_core::filter::parse_cql2;
use maptile_core::prelude::*;

async fn read_primary_roads(stack: &TileStack) -> MaptileResult<TileOutcome> {
    let filter = parse_cql2("class = 'primary'")?;
    let query = TileQuery {
        filter: Some(&filter),
        bbox: Some([115.4, 39.4, 117.6, 41.1]),
        ..TileQuery::default()
    };
    let mut target = ReadTarget::new("roads", "public");
    target.query = Some(&query);
    stack.read(target.at(TileCoord { z: 10, x: 843, y: 388 })).await
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

Postgres 表源和函数源不持有瓦片结果缓存。通过 `CatalogService` 配置缓存并使用 `TileStack` 读取；`cache_zoom` 控制缓存层级。直接调用源会重新查询数据库。旧 `SourceManager::with_cache` 仍可用于兼容调用方。

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

显式 `bounds` 的四个分量必须有限且南纬不大于北纬，否则打开源时返回 `MaptileError::Config`。未指定或非正数 `srid` 沿用 WGS84，纬度必须在 `[-90, 90]` 内，包含两极和等边点范围。正数非 4326 的 SRID 先通过 PROJ 投影，再检查 WGS84 输出域，不把投影坐标当作纬度。展开经度仍可输入，例如 `190..200` 对外公布为 `-170..-160`；跨日期线或覆盖整圈时，单个 TileJSON bbox 用 `[-180, 180]` 包住两侧。这样 bounds 符合 [TileJSON 3.0 的坐标域](https://github.com/mapbox/tilejson-spec/blob/master/3.0.0/README.md#35-bounds)，center 位于公布的范围内。`MosaicSource` 由 `agtiff` feature 提供。

跨日期线可以用 `west > east` 表示，也可提供递增的未折回经度，例如 `175..185`。成员范围未知时可用 `[0; 4]`；非有限、倒置纬度、越出纬度范围或退化 bbox 都保守地作为所有瓦片的候选，并计入上限。因此 65 个未知范围仍可能超限，应尽量提供准确的成员范围。接近 360° 的跨度如果在折回时舍入为同一经度，会保守覆盖整条纬度带，避免漏选成员。未显式配置合并范围时，未知成员会扩大 TileJSON 范围到世界，跨日期线成员会使单个 TileJSON bbox 覆盖两侧。

全局统计仍使用完整、有序的成员集，不能用单瓦片候选替代。锁定的 async-geotiff 0.13.0 在统计发现阶段也应用默认 64 个资产的限制，超过 64 个成员的全局统计拉伸仍不受支持；需要统计的样式还会读取远处成员，远处坏文件可能使统计失败。普通固定样式下，65 个成员中只有 1 个相交的场景已由真实投影 TIFF 回归覆盖。

需要跨页一致导出时，通过 `MosaicSource::open_immutable` 打开，调用方须保证每个成员都是不可变版本，且重叠成员次序固定。普通 `open` 提供按请求的瓦片读取。内容版本包含成员身份和归一化范围，修改范围后重新打开并注册会分开瓦片缓存。成员内容本身变化时，应用仍需更新其 `source_id`/内容身份。配置与真实样式示例见[栅格渲染](rendering.md#stac固定镶嵌与-outdb)及 [`mosaic.rs`](../src/sources/raster/mosaic.rs)。

## FlatGeobuf 文件版本

FlatGeobuf 在 Unix 上将完整 FileStamp（含设备号、inode、ctime、长度和 mtime）纳入内容版本，同长度、同 mtime 的不同 inode 替换文件重新打开后也获得不同版本。Windows 的读取检查和内容版本包括卷、文件 ID 和变更时间。其余平台每次打开分配新代次，克隆共享版本。更新文件后仍须重新打开和注册；热缓存命中不会自动执行文件变化检查。查询与关闭流程见[矢量文件指南](geoparquet.md#读取本地几何数据)。

## 组合多个 MVT 源

通过目录更新发布 `CompositeDeclaration`。各成员须为 MVT、保证合规且完整声明图层名；未知图层集合的源不能加入严格组合。发布时检查缺失成员、循环、重复叶子和深度限制。成员顺序固定在目录快照中。

```rust
use maptile_core::prelude::*;

fn register_city_layers(catalog: &CatalogService) -> MaptileResult<()> {
    let mut update = catalog.catalog().update();
    update.composite(CompositeDeclaration {
        id: "city".into(),
        members: vec!["roads".into(), "buildings".into()],
        lazy: false,
        duplicate_layer: DuplicateLayer::Error,
        failure_policy: MemberFailurePolicy::FailFast,
    })?;
    catalog.catalog().publish(update)?;
    Ok(())
}
```

普通未信任的外部归档可独立读取，不能仅凭一张抽样瓦片推定其所有坐标合规。归档生产端能保证所有坐标的 MVT 合规并完整声明层名时，应用可通过 `put_trusted_mvt` 明确声明该信任契约；其余来源维持普通 `put`。

默认 `DuplicateLayer::Error` 拒绝重复图层。`KeepFirst` 和 `KeepLast` 按每张瓦片实际出现的同名层裁决。由 0.3 迁移且依赖后层覆盖时可选择 `KeepLast`。严格组合不做运行时 `layer_prefix` 改写；需先调整生产端图层名与样式引用。

`lazy: true` 允许目录发布时缺少成员，但保留完整声明参与环检查。全部成员缺失时组合不可用。默认 `FailFast` 遇到故障返回错误；`SkipTransient` 可跳过暂时不可用的成员并返回 `Partial`。全部成员故障仍返回错误。无效输入、能力不支持、取消及超限不降级。部分结果不进入规范缓存，HTTP 层必须使用 `no-store` 且不生成强 ETag。

组合成员共享一次请求的字节预算。`CompositeLimits::max_work_bytes` 限制压缩输入、解码载荷和组合输出合计可预留的字节数。每个成员在解压完成后重新检查预算，预留解码载荷并释放压缩缓冲，再校验实际图层。多个成员并行校验时，MVT 解析器的临时堆分配可以叠加。内存紧张时，调低 `max_concurrency` 可减少同时处理的成员，设为 1 可串行读取和校验。过滤等条件必须得到所有成员支持。按逗号分隔 ID 的临时组合在当前快照内编译，不写入全局目录。

## 格式与压缩编码

`TileResponse.info.format` 描述数据格式，`info.encoding` 描述本次响应字节的压缩方式。将两者与载荷一起传递，可以让客户端、转码器和组合源使用正确的解码路径。

[`transport`](../src/transport.rs)提供显式 gzip、zlib、brotli、zstd 编解码，以及异步包装。启用 `mlt` 后，[`transcode`](../src/transcode/mod.rs)提供 MVT 与 MLT 转换。`transcode_with_gzip` 可自动识别 gzip、zstd、zlib；Brotli 载荷先通过已知编码调用 `transport::decode`，再传入转码器。

DuckDB 几何切片与预编码 Parquet 归档的边界和预算见 [GeoParquet 指南](geoparquet.md)。

## 数据更新与关闭

1. 准备具有明确版本身份的新文件、对象或数据库修订号。
2. 打开并校验新源，通过 `catalog.update()` 和 `put` 准备变更，再调用 `publish` 一次发布。
3. 等待持有旧快照的请求和导出会话结束，再回收旧文件。紧急撤销时使用 `revoke` 并等待排空。

`reconcile` 可根据完整或部分发现清单更新绑定。完整清单会下线本命名空间中不再出现的源，并保留其数据集绑定，部分清单不会删除；失败清单保留旧目录。具体语义见[目录发布与生命周期](architecture.md#目录发布与生命周期)。
