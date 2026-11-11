# GeoParquet 独立使用

启用 `geoparquet` feature 后，核心库可读取预编码 `(z,x,y,tile)` 归档，或直接从几何 GeoParquet 按需生成 MVT。它不依赖 MapSeek 服务、数据库配置或业务身份。

## 构建与扩展

当前锁文件使用 `duckdb = 1.10505.0`，对应嵌入式 DuckDB 1.5.5，Parquet 随库编译。几何模式需要匹配该版本及平台的官方签名 Spatial 扩展，远程模式还需要 HTTPFS。

```sh
# 检查已有扩展，不下载。
cargo run --locked --features geoparquet --example geoparquet_extensions -- --check
# 明确准备官方扩展。以实际运行程序的用户执行。
cargo run --locked --features geoparquet --example geoparquet_extensions -- --install
```

`--install` 只使用 DuckDB 官方 core 仓库，加载时校验签名。构造数据源仅执行 LOAD，不在请求中安装或升级扩展；缺失扩展会报错，不静默降级。不支持配置任意扩展来源或 SQL 模板。

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

`geometry_column = None` 保留旧归档模式，在打开时建立内存瓦片索引。远程打开只支持几何模式，必须指定 `geometry_column`。

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
