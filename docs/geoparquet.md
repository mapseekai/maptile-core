# GeoParquet 独立使用

启用 `geoparquet` feature 后，核心库可读取预编码 `(z,x,y,tile)` 归档，或直接从几何 GeoParquet 按需生成 MVT。它不依赖 MapSeek 服务、数据库配置或业务身份。

## 构建与扩展

当前锁文件使用 `duckdb = 1.10506.1`，对应嵌入式 DuckDB 1.5.6。Parquet 随库编译。几何模式需要匹配该版本及平台的官方签名 Spatial 扩展，远程模式还需要 HTTPFS。

```sh
# 检查已有扩展，不下载。
cargo run --locked --features geoparquet --example geoparquet_extensions -- --check
# 明确准备官方扩展。以实际运行程序的用户执行。
cargo run --locked --features geoparquet --example geoparquet_extensions -- --install
```

`--install` 只使用 DuckDB 官方 core 仓库，加载时校验签名。构造数据源仅执行 LOAD，不在请求中安装或升级扩展；缺失扩展会报错，不静默降级。不支持配置任意扩展来源或 SQL 模板。

内置下载受网络限制时，可通过官方 HTTPS 地址下载对应版本和平台的签名扩展，再用同版本 DuckDB 从本地文件执行 `INSTALL`。 [手动安装说明](https://duckdb.org/docs/1.5/extensions/troubleshooting)

## 本地几何源

将以下代码放入启用了 `geoparquet` 和 Tokio 的调用方。同步打开使用 blocking pool，瓦片查询异步执行：

```rust
use maptile_core::sources::geoparquet::{GeoParquetSource, GeoParquetSourceConfig};
use maptile_core::sources::{TileCoord, TileSource};

async fn read_tile() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let config = GeoParquetSourceConfig {
        id: "places".into(),
        path: std::path::PathBuf::from("/data/places.parquet"),
        geometry_column: Some("geometry".into()),
        layer_name: "places".into(),
        id_column: Some("id".into()),
        ..Default::default()
    };
    let source = tokio::task::spawn_blocking(move || GeoParquetSource::open(config)).await??;
    let result = source.get_tile(TileCoord { z: 2, x: 2, y: 1 }, None).await;
    source.close().await?;
    let tile = result?;
    println!("{} bytes", tile.data.len());
    Ok(())
}
```

`geometry_column = None` 使用预编码归档模式。打开时只读取配置的 `z`、`x`、`y` 和瓦片列，并建立内存索引。这四列使用无压缩的 Parquet 列块，瓦片内容可按 `encoding` 压缩。归档最多扫描 400 万行。单瓦片上限为 `query_options.max_tile_bytes`，索引及瓦片使用 `query_options.memory_limit_mb` 的一半作为预算，其余用于读取和解码。超出上限时打开失败。远程打开只支持几何模式，必须指定 `geometry_column`。

## HTTP 和对象存储

- `GeoParquetSource::open_http(config, url).await`：有界 HTTP(S) Range 读取。默认客户端禁止自动重定向、环境代理及透明解压。
- `open_http_with_client(config, url, client).await`：注入调用方的 reqwest 客户端。调用方负责同样的重定向/解压限制，以及 DNS 固定和网络出口策略。
- `open_object_store(config, store, key).await`：通过已有的 `Arc<dyn object_store::ObjectStore>` 读取 S3 兼容或其他对象存储。凭据、桶和端点由调用方授权与配置。

远程模式不会先把整个文件落到本地缓存。核心在 loopback 上创建源专属、带随机会话令牌的范围读取器，DuckDB HTTPFS 只接触本机 URL，真实签名查询串和对象存储凭据不会进入 SQL。进程需要允许绑定 loopback listener。

**核心库不提供 MapSeek 服务层的 SSRF 防护。** 独立调用方必须先授权目标地址，并限制私网、云元数据地址、DNS 重绑定及网络出口。不要直接把不可信用户提供的 URL 传给 `open_http`。提供自定义客户端不会自动继承服务工厂的安全策略。

## 几何与属性契约

- 几何列必须为可读取的 WKB/GEOMETRY；当前不支持 GeoArrow 原生几何编码或 spherical edges。
- `srid` 可提供明确的 EPSG 覆盖值。否则读取 GeoParquet PROJJSON 的 EPSG/OGC:CRS84 标识；省略 crs 等同 CRS84，显式 `crs: null` 需要覆盖值。兼容旧接口：明确指定几何列但没有 geo 元数据的 WKB Parquet 默认 WGS84。
- 几何先按 always_xy 投影到 EPSG:3857，再相交、裁剪和 MVT 编码。默认 extent=4096、buffer=64；extent 需为 1..=65536，buffer 不超过 extent。zoom 需为有序的 0..=30。
- `properties = None` 选择 MVT 可表达的标量字段，`Some(...)` 是字段白名单。容器、二进制及其他几何字段不自动输出。
- 整数 ID 转为非负 BIGINT；负值不编码为 ID，超过有符号 64 位上限报错。普通 UInt64 属性输出十进制字符串以保留精度。
- 日期、时间和 UUID 输出字符串，DECIMAL 输出浮点数。无法安全推导元数据 bounds 时省略，不在启动时扫描全表。
- 空数据/无命中得到成功的空瓦片；无效坐标及超出 zoom 范围得到 TileNotFound。

GeoParquet 1.1 `covering.bbox` 可用于行组裁剪。仅对可解析为数值字段的路径及 4326/3857 启用，以免其他投影造成漏要素。没有合法 covering 仍可查询，但每瓦片可能扫描所有行组。生产者必须保持 covering 与几何一致，核心不全量验证每行 bbox。

## 预算和关闭

`GeoParquetSourceConfig.query_options` 的默认值：

```text
pool_size = 2
threads = 2
memory_limit_mb = 256
timeout_ms = 10000
max_features = 100000
max_tile_bytes = 8388608
```

预算按源共享。DuckDB memory_limit 是查询内存限制，不是进程 RSS 上限；多个活跃源仍会占用各自 listener、连接和文件描述符，应用需限制源数量。

所有 source 克隆共享会话。`source.close().await` 幂等，会拒绝新请求、打断排队和在途查询，并等待本机读取器与查询资源退出。取消一个 close future 后，可用保留的 source 再次 await close。会话只释放自己持有的客户端引用，不强制关闭其他源共用的客户端。

使用 SourceManager 时可调用 `close_source(id).await`：移除源、失效热缓存并等待关闭。普通 remove、不可用标记和替换注册使用非等待式 start_close；显式等待仍应使用 close_source。Drop 发起关闭，但不是资源已完全释放的确认。

使用不可变、带版本的 URI；替换数据时更新 URI 并重新注册。网络读取验证 ETag/Last-Modified 或对象版本，但缓存命中未必再访问远端；没有 validator 的 HTTP 文件尤其必须保持不变。本地路径会核对文件大小和 mtime，变化时需要重新打开。

## 验证

```sh
cargo test --locked --features geoparquet --lib
cargo clippy --locked --all-targets --features geoparquet -- -D warnings
# 手动微基准，非生产性能保证。
cargo test --locked --features geoparquet --lib repeated_tile_probe -- --ignored --nocapture
```

核心测试用合成 GeoParquet 验证投影、属性/ID、跨边界几何、covering、查询预算、HTTP Range、源管理和会话关闭。它们不能证明真实 AWS 端点、MapSeek 服务工厂、生产负载或跨平台行为；服务层 S3 集成测试留在 MapSeek。

## 本地 FlatGeobuf 与一次性导入

启用 `local-vector` feature（包含 `geoparquet`）后，从 `sources` 或 `prelude` 引入公开类型。读取使用 DuckDB Spatial 自带的 GDAL 驱动，需预装与嵌入式 DuckDB 匹配的官方签名 Spatial 扩展。

```rust
use maptile_core::sources::{FlatGeobufSource, FlatGeobufSourceConfig, TileCoord, TileSource};

async fn read_flatgeobuf() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let source = tokio::task::spawn_blocking(|| FlatGeobufSource::open(FlatGeobufSourceConfig {
        id: "places".into(),
        path: "/data/places.fgb".into(),
        layer_name: "places".into(),
        id_column: Some("id".into()),
        ..Default::default()
    })).await??;
    let result = source.get_tile(TileCoord { z: 2, x: 2, y: 1 }, None).await;
    source.close().await?;
    println!("{} bytes", result?.data.len());
    Ok(())
}
```

`FlatGeobufSource` 只读取一个本地常规文件，不接受 URL、glob、SQL 或驱动参数。几何列由驱动发现；CRS 必须有可信的 EPSG / OGC:CRS84 标识，否则需要 `srid` 显式覆盖。几何投影到 EPSG:3857 后，按含 buffer 的瓦片范围精确相交与裁剪，保留跨瓦片、世界边缘及投影后的线段。

**当前锁定的 Spatial 1.5.6 对 FlatGeobuf 使用完整扫描。** 该版本的索引筛选可能漏掉投影后的长线，并在空命中时产生无效批次。有索引和无索引的文件均可直接切片，大文件的单瓦片查询可能超过预算。连接池、资源预算、文件变化检查、文件 lease、缓存版本及 close 语义与 GeoParquet 相同。

GeoJSON 和 ZIP Shapefile 通过 `import_to_geoparquet` 一次性转换，瓦片查询只读取导入后的 GeoParquet：

```rust
use maptile_core::sources::{
    GeoParquetSource, GeoParquetSourceConfig,
    VectorImportFormat, VectorImportOptions, import_to_geoparquet,
};

async fn import_places() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let imported = import_to_geoparquet(
        "/data/places.zip",
        VectorImportFormat::ShapefileZip, // GeoJSON 使用 GeoJson
        "/data/places.parquet",
        VectorImportOptions::default(),
    ).await?;
    let source = tokio::task::spawn_blocking(move || GeoParquetSource::open(GeoParquetSourceConfig {
        id: "places".into(),
        path: imported.path,
        geometry_column: Some(imported.geometry_column),
        ..Default::default()
    })).await??;
    source.close().await?;
    Ok(())
}
```

导入将几何规范为 WGS84 WKB，移除 M 维度并保留 Z。输出保留驱动读到的属性和 GeoParquet 1.1 `covering.bbox`，NULL 几何对应 NULL bbox。返回值给出实际几何列名。若属性已经占用 `geometry`，几何列会追加下划线。驱动无法解析的字段或驱动列名冲突会报错，例如原属性 `wkb_geometry` 与 Spatial 生成的 WKB 列同名。正常 GeoJSON 使用 WGS84，Shapefile 通过 `.prj` 识别 CRS，未知 CRS 需要 `VectorImportOptions.srid`。提取后检查 `.shp/.shx` 文件头长度、索引记录数及 `.dbf` 记录体积。各条几何记录的解析由 GDAL 完成。

ZIP 可含嵌套目录，但必须恰好包含一套同路径、同 stem 的 `.shp/.shx/.dbf`，可另含 `.prj/.cpg`；大小写不敏感。只提取这五类组件。拒绝路径逃逸、绝对路径、链接、特殊文件、重复组件、多个 Shapefile、加密和非 Stored/Deflate 压缩。支持普通单卷 ZIP，最多 128 个条目、1 MiB 中央目录，不支持 ZIP64 目录或尾部附加数据；在分配完整条目索引前检查。ZIP 元数据和实际解压字节均受限。

默认导入预算为 512 MiB、30 秒、两个 DuckDB 线程及 256 MB 的 DuckDB 管理内存，禁止磁盘 spill。`max_bytes` 分别约束输入、ZIP 声明的总解压体积、实际提取字节及完成后的输出；输出大小在 COPY 完成后检查，因此它不是磁盘配额。`timeout_ms` 允许 1..=300000，`max_bytes` 允许 1..=8 GiB。应用仍需限制并行导入数量及磁盘使用。

目标父目录必须由调用方控制。输入快照和输出位于该目录内的私有临时目录（Unix 权限 0700）；发布文件的权限遵循进程 umask。转换完整成功后才通过同文件系统的硬链接原子发布；目标必须不存在，文件系统需支持硬链接。失败或 future 被取消时不发布输出，阻塞任务退出后清理暂存文件。成功结果由调用方持有，之后可以删除原始 GeoJSON / ZIP。调用方负责授权输入、目标目录及本地文件的访问，保持直接服务的 FlatGeobuf 不变。

以下示例分别读取三种格式的世界瓦片，并打印生成的 MVT 字节数。导入命令使用尚未创建的输出路径。

```sh
cargo run --locked --features local-vector --example local_vector -- fgb places.fgb
cargo run --locked --features local-vector --example local_vector -- geojson places.geojson places-json.parquet
cargo run --locked --features local-vector --example local_vector -- shapefile places.zip places-shp.parquet
```

```sh
cargo test --locked --features local-vector --lib
cargo clippy --locked --all-targets --features local-vector -- -D warnings
```

`geoparquet` CI 检查名保持不变，启用 `local-vector` 并覆盖原有 GeoParquet 与新增格式测试。本地用 Spatial 生成合成 FGB / Shapefile，不依赖系统 GDAL、外部服务或真实业务数据。
