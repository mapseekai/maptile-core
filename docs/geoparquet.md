# 从 GeoParquet 与 FlatGeobuf 提供瓦片

启用 `duckdb` 或 `geoparquet`，通过 DuckDB Spatial 从几何文件按需生成未压缩 MVT。已经编码的 Parquet 瓦片使用独立的 `parquet-archive` feature。两种入口均提供 Web Mercator XYZ。

| 输入 | 入口 |
| --- | --- |
| 本地 GeoParquet | `GeoParquetSource::open` |
| 不可变的 HTTP(S) GeoParquet | `open_immutable_http` 或 `open_immutable_http_with_headers` |
| 由强 ETag 固定的 HTTP(S) GeoParquet | `open_http_at_etag` |
| 不可变的 S3 GeoParquet | `open_immutable_s3` 或 `open_immutable_s3_with_provider` |
| 本地 FlatGeobuf | `FlatGeobufSource::open` |
| 本地预编码 Parquet | `ParquetArchiveSource::open` |

## 准备 DuckDB 扩展

[Cargo.lock](../Cargo.lock) 锁定 `duckdb 1.10506.1`，对应 DuckDB 1.5.6。Parquet 随 DuckDB 编译。几何源需要匹配版本和平台的签名 Spatial 扩展。直接查询不可变 HTTP 或 S3 对象时还需要 HTTPFS。

使用运行应用的系统账户检查扩展。首次部署时，将 `--check` 换成 `--install`，从 DuckDB 官方 core 仓库安装匹配的签名扩展。数据源只加载已经安装的扩展。

```sh
cargo run --locked --features duckdb --example geoparquet_extensions -- --check
```

## 读取本地几何数据

在异步应用中，通过 `control::open_blocking` 打开本地源。在目录发现过程中调用时，目录会等到后台文件操作结束才完成关闭。以下代码读取 `/data/places.parquet` 中的主几何列，生成一张 `places` 图层的 MVT，并等待连接和文件租约释放。根据文件列名设置 `id_column`，也可以省略该字段。

```rust
use maptile_core::control::open_blocking;
use maptile_core::sources::{GeoParquetSource, GeoParquetSourceConfig, TileCoord, TileSource};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let source = open_blocking(|| {
        GeoParquetSource::open(GeoParquetSourceConfig {
            id: "places".into(),
            path: "/data/places.parquet".into(),
            layer_name: "places".into(),
            id_column: Some("id".into()),
            ..Default::default()
        })
    })
    .await?;

    let result = source.get_tile(TileCoord { z: 2, x: 2, y: 1 }, None).await;
    source.close().await?;
    println!("{} bytes of MVT", result?.data.len());
    Ok(())
}
```

FlatGeobuf 使用相同的瓦片接口。用 `FlatGeobufSourceConfig` 指定本地文件路径和图层，然后调用 `FlatGeobufSource::open`。该入口检查驱动、图层数和几何列数，并读取 CRS。需要将同一个 FlatGeobuf 文件转换为独立的 GeoParquet 时，用 `import_to_geoparquet(input, output, options)`。转换完成后原子发布目标文件，目标路径必须空闲。在线切片和此转换入口不接收 GeoJSON 或 ZIP Shapefile；先在上游转换为 GeoParquet 或 FlatGeobuf。

```sh
cargo run --locked --features duckdb --example local_vector -- fgb places.fgb
cargo run --locked --features duckdb --example local_vector -- import places.fgb places.parquet
```

### CRS、属性与空间过滤

| 配置 | 行为 |
| --- | --- |
| `geometry_column` | 默认读取 GeoParquet 元数据中的主几何列。可以覆盖为文件中声明的另一几何列。 |
| `srid` | 显式指定 EPSG。GeoParquet 缺省 CRS 为 `OGC:CRS84`，显式 `null` 需要配置 EPSG。 |
| `layer_name` | 默认为 `data`。 |
| `id_column` | 选择整数属性作为 MVT ID。当前 Spatial 路径保留 `0..=i64::MAX`，其他值省略 ID。 |
| `properties` | 默认选择可编码的标量列。有列白名单时，打开源会验证每一列。 |
| `extent` / `buffer` | 默认 4096 / 64。边缘缓冲按 MVT 坐标计算。 |
| `bounds_mode` | `Skip` 为默认值。缺少显示范围时，`Quick` 尝试扫描最多 4096 行，必要时在同一期限内进行完整扫描。`Calc` 直接扫描完整数据。超时保留未知显示范围。 |
| `trust_covering` | 默认关闭。确认生产端正确生成 covering 并使用标准 CRS 坐标后，可对 4326 和 3857 使用空间剪枝。 |

文件范围与扫描结果仅用于显示。查询先以带缓冲的瓦片空间范围筛选，再以 EPSG:3857 的几何相交结果生成 MVT。FlatGeobuf 使用精确扫描。4326 的瓦片缓冲区触及反经线时，查询也使用精确扫描。

`TileQuery::bbox` 接收 WGS84 范围。`TileQuery::filter` 支持输出字段上的标量比较、逻辑组合、`IN`、`LIKE` 和空值检查。过滤值使用 SQL 参数绑定。

布尔值保留原类型，普通整数和浮点以 MVT 数值表示。`DECIMAL` 转换为双精度，可能损失精度。日期时间与超大整数用字符串表示。默认略过嵌套字段，TileJSON 的 `vector_layers` 只列出实际输出字段。

## 固定远程对象版本

地址随版本发布且内容不可变时，使用 `open_immutable_http` 或 `open_immutable_s3`。DuckDB HTTPFS 在查询时直接按需读取远程范围。部署侧负责约束网络出口和重定向。未限定版本的 `open_http`、`open_http_with_headers` 和 `open_s3` 返回 `Unsupported`。

HTTP 地址内容会变化且已知所需版本的强 ETag 时，使用 `open_http_at_etag`。打开时客户端从最终 URL 每次最多读取 1 MiB，发送 `If-Match`，检查每个 206 响应的 ETag、字节范围、对象总长度和实际载荷。请提供最终地址，重定向响应会使打开失败。版本不符、缺少验证值或收到 404、410、412 时返回 `VersionChanged`。

所有分段通过检查后，DuckDB 从系统临时目录的私有文件读取。源站随后替换或删除文件时，已打开的源仍可读出原版本。每个源最多占用 `query_options.memory_limit_mb` MiB 临时磁盘，默认 256 MiB，这部分磁盘与 DuckDB 内存分别计量。`timeout_ms` 同时约束下载和打开。调用 `close().await` 排空查询后会释放临时文件。

`open_immutable_http_with_headers` 和 `open_http_at_etag` 可接收认证请求头，Host、Range 和传输头由客户端管理。`GeoParquetS3Options` 指定 S3 认证和端点。`open_immutable_s3_with_provider` 按源缓存临时凭据，合并并发刷新。后端借用连接时检查凭据，到期且刷新失败时拒绝此次借用。应用应先确定请求的授权范围，再读取目录。撤销已发布源的访问权时，对观察到的快照调用 `catalog.revoke(&snapshot, source_id, deadline).await`。这会同时停止该源及依赖它的组合的缓存读取。直接管理独立源时，可调用 `start_close()` 停止后端准入，再等待 `close().await`。

```sh
cargo run --locked --features duckdb --example geoparquet_remote -- --immutable 'https://example.org/data/revision-1.parquet'
```

## 配置资源与关闭源

`GeoParquetQueryOptions` 应用于两种几何源。克隆共享同一池。调用 `pool_status()` 查看准入状态、活跃请求、工作额度和 deadpool 连接数。活跃请求包含排队、建连、回收和中断后仍在运行的工作。

| 配置 | 默认值 | 预算 |
| --- | --- | --- |
| `pool_size` | 2 | 独立内存实例数，范围 1..=32 |
| `threads` | 2 | 每个实例的 DuckDB 线程数，范围 1..=64 |
| `memory_limit_mb` | 256 | DuckDB 分给各实例的总内存，每实例至少 16 MB，按 ETag 打开时还以此数值单独限制快照磁盘 MiB |
| `timeout_ms` | 10000 | 每次查询的排队、建连及执行期限，远程打开时的下载和元数据读取也共用这个时限 |
| `max_features` | 100000 | 单瓦片候选数，超过预算返回错误 |
| `max_tile_bytes` | 8 MiB | 单瓦片输出大小，在数据库中检查 |

从配置的总内存中还需区分 DuckDB 内部预算与实际进程 RSS。进程包含扩展、网络和查询缓冲区。部署时同时测量新旧版本和活跃源的并行内存。

本地文件发布后保持不可变。读取前后核对文件标识，文件更换或消失时返回 `VersionChanged`。重新打开源会捕获文件新版本。调用 `with_file_lease` 将应用拥有的文件租约交给源。即使请求退出，阻塞 worker 仍持有连接、配额和文件租约，直到实际退出。

`start_close` 停止准入并取消在途查询。`close().await` 等待建连、回收、查询及其文件租约退出。可以通过源的任意克隆再次等待关闭。

## 读取预编码 Parquet 归档

启用 `parquet-archive` 并调用 `ParquetArchiveSource::open`。Arrow 从 `z`、`x`、`y` 和二进制 `tile` 四列建立只读内存索引。这四列的 Parquet 列块使用未压缩格式。通过 `ParquetArchiveSourceConfig::format` 和 `encoding` 声明归档内瓦片的实际格式和编码。

`memory_limit_mb` 限制元数据、行组和索引的预算。`max_tile_bytes` 限制单个瓦片的大小。索引打开后拥有所读字节，源切换时为新文件打开另一个索引。该 feature 的构建和读取均无需 DuckDB。

## 验证本地构建

```sh
cargo test --locked --features duckdb --lib
cargo test --locked --features parquet-archive --lib sources::parquet_archive
cargo clippy --locked --all-targets --features duckdb,parquet-archive
```
