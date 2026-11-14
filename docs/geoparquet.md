# GeoParquet 与本地矢量数据

`maptile-core` 支持从 GeoParquet 几何数据按需生成 MVT，也支持读取已经编码好的瓦片归档。启用 `local-vector` 后，还可以直接读取本地 FlatGeobuf，或将 GeoJSON、ZIP Shapefile 导入为 GeoParquet 后提供瓦片。

本指南面向在 Rust 应用中接入文件数据源的开发者。公开接口见 [GeoParquet 源](../src/sources/geoparquet.rs)和[本地矢量源](../src/sources/local_vector.rs)，整体接入方式见[快速开始](getting-started.md)。

## 选择数据入口

| 数据 | 入口 | 处理方式 | Feature |
| --- | --- | --- | --- |
| 本地 GeoParquet 几何文件 | `GeoParquetSource::open`，设置 `geometry_column` | 查询几何、投影、裁剪并生成 MVT | `geoparquet` |
| HTTP(S) GeoParquet | `open_http` / `open_http_with_headers` | 按 Range 读取远程几何文件并生成 MVT | `geoparquet` |
| S3 兼容对象存储中的 GeoParquet | `open_s3` | DuckDB HTTPFS 直接按范围读取 | `geoparquet` |
| 本地预编码瓦片归档 | `GeoParquetSource::open`，使用默认 `geometry_column = None` | 从 `(z, x, y, tile)` 列建立内存索引，按坐标返回瓦片 | `geoparquet` |
| 本地 FlatGeobuf | `FlatGeobufSource::open` | 直接读取几何并生成 MVT | `local-vector` |
| 本地 GeoJSON、ZIP Shapefile | `import_to_geoparquet` | 一次性转换为 GeoParquet，再通过几何源提供瓦片 | `local-vector` |

几何源按 `TileCoord` 指定的 XYZ 坐标生成未压缩 MVT。图层名、属性列、要素 ID 和几何裁剪参数在打开源时配置。

## 准备构建环境与 DuckDB 扩展

在调用方的依赖中启用所需 feature：

```toml
[dependencies]
maptile-core = { git = "https://github.com/mapseekai/maptile-core.git", features = ["geoparquet"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

需要 FlatGeobuf 或文件导入时，将 feature 改为 `local-vector`，它会同时启用 `geoparquet`。构建使用随 crate 编译的 DuckDB，需准备 C/C++ 工具链；通用环境说明见[构建与验证](development.md)。

[Cargo.lock](../Cargo.lock) 锁定 `duckdb 1.10506.1`，对应嵌入式 DuckDB 1.5.6。Parquet 随库编译；几何查询需要匹配版本和平台的官方签名 Spatial 扩展，远程读取还需要 HTTPFS。FlatGeobuf 与导入流程使用 Spatial 内置的 GDAL 驱动。

以实际运行应用的用户，在仓库根目录执行[扩展准备示例](../examples/geoparquet_extensions.rs)：

```sh
cargo run --locked --features geoparquet --example geoparquet_extensions -- --check
```

此命令检查本机已有的 Parquet、Spatial 和 HTTPFS，并输出嵌入式 DuckDB 版本及平台。首次准备环境时执行：

```sh
cargo run --locked --features geoparquet --example geoparquet_extensions -- --install
```

安装命令从 DuckDB 官方 core 仓库获取匹配的扩展，加载时校验签名。数据源构造阶段通过 `LOAD` 使用已准备的扩展，扩展缺失会返回加载错误。SQL 由核心根据数据源配置生成。

也可以通过 DuckDB 官方 HTTPS 地址获取匹配版本和平台的签名扩展，再用同版本 DuckDB 从本地文件执行 `INSTALL`，操作方式见 [DuckDB 手动安装说明](https://duckdb.org/docs/1.5/extensions/troubleshooting)。

## 读取本地 GeoParquet

准备包含 `geometry` 几何列和整数 `id` 列的 `/data/places.parquet`。以下程序打开数据源、读取一张瓦片并等待源关闭：

```rust
use maptile_core::sources::{GeoParquetSource, GeoParquetSourceConfig, TileCoord, TileSource};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let config = GeoParquetSourceConfig {
        id: "places".into(),
        path: "/data/places.parquet".into(),
        geometry_column: Some("geometry".into()),
        layer_name: "places".into(),
        id_column: Some("id".into()),
        ..Default::default()
    };
    let source = tokio::task::spawn_blocking(move || GeoParquetSource::open(config)).await??;

    let result = source.get_tile(TileCoord { z: 2, x: 2, y: 1 }, None).await;
    source.close().await?;
    let tile = result?;
    println!("{} bytes of MVT", tile.data.len());
    Ok(())
}
```

根据实际文件调整路径、几何列和 ID 列。读取成功后，程序输出该瓦片的字节数；目标范围内没有要素时，几何源成功返回空瓦片。

`open` 是同步接口，异步应用通过 `spawn_blocking` 完成打开操作。几何模式在启动时读取 schema 和文件尾元数据，瓦片请求时执行空间查询。需要通过统一入口管理多个源时，将源包装为 `Arc` 后交给 `SourceManager::register`，具体流程见[架构与集成](architecture.md)。

### 配置字段

以下默认值来自 [`GeoParquetSourceConfig`](../src/sources/geoparquet.rs)。

| 字段 | 默认值 | 用途 |
| --- | --- | --- |
| `id` | `"geoparquet"` | 源管理器使用的数据源标识 |
| `path` | 空路径 | 本地文件路径；远程入口通过独立参数接收 HTTP(S) URL 或 S3 URI |
| `geometry_column` | `None` | 设置为几何列名时启用动态 MVT；保留默认值时读取瓦片归档 |
| `layer_name` | `"data"` | 动态 MVT 内部的图层名 |
| `srid` | `None` | 显式 EPSG 覆盖值，默认读取文件的 CRS 信息 |
| `id_column` | `None` | 用作 MVT 要素 ID 的整数列 |
| `properties` | `None` | 默认选择可编码的标量属性；`Some(vec![...])` 指定属性白名单 |
| `extent` | `4096` | MVT 坐标空间大小，范围为 `1..=65536` |
| `buffer` | `64` | 瓦片边缘缓冲区，单位与 extent 一致，取值为 `0..=extent` |
| `minzoom` / `maxzoom` | `0` / `22` | 服务的缩放级别，满足 `0 <= minzoom <= maxzoom <= 30` |
| `bounds` | `None` | TileJSON 的 WGS84 范围；几何模式可从有效元数据推导 |
| `name` / `attribution` / `description` | `None` | TileJSON 名称、来源署名和说明 |
| `columns` | `z`、`x`、`y`、`tile` | 预编码归档的坐标列和瓦片列名称 |
| `format` / `encoding` | MVT / 未压缩 | 预编码归档中保存的瓦片格式与编码 |
| `query_options` | 见[查询预算](#查询预算) | 连接数、线程数、内存、期限和瓦片大小预算 |
| `cache_zoom` | `None` | 默认允许各缩放级别进入管理器缓存，可配置 `CacheZoomRange` |

## 读取 HTTP 和对象存储中的 GeoParquet

远程入口使用几何模式，在配置中设置非空的 `geometry_column`。DuckDB HTTPFS 直接读取远程 Parquet，Spatial 在同一查询中完成空间过滤、投影、裁剪和 MVT 编码。核心库不启动本机 HTTP 服务，也不把整个文件预先下载到本地。

| 接口 | 调用方提供 | 认证方式 |
| --- | --- | --- |
| `open_http(config, url).await` | 单个 HTTP(S) 文件 URL | 公共 URL 或预签名 URL |
| `open_http_with_headers(config, url, headers).await` | 文件 URL 与 `BTreeMap<String, String>` | HTTPFS 会话内的 HTTP secret，例如 Authorization 头 |
| `open_s3(config, uri, options).await` | `s3://bucket/key` 与 `GeoParquetS3Options` | HTTPFS 会话内的 S3 secret，支持自定义 endpoint、临时令牌与 path-style |

三个入口只接受单个文件，不接受通配符、目录、userinfo、反斜杠或 fragment；S3 URI 不接受查询串。协议名统一为小写，HTTP 路径和查询串保留原文，以兼容预签名 URL。HTTP 认证头不能覆盖 Range、Host、Content-Length、Accept-Encoding 或对象校验头。`GeoParquetS3Options` 默认使用 `us-east-1`、HTTPS 和虚拟主机寻址；空的 access key/secret key 表示匿名访问，非空时必须同时提供。`endpoint` 只填写主机与可选端口，不包含 scheme 或路径。

```rust
use maptile_core::sources::{
    GeoParquetSource, GeoParquetSourceConfig, TileCoord, TileSource,
};

async fn read_remote() -> Result<(), Box<dyn std::error::Error>> {
    let config = GeoParquetSourceConfig {
        id: "places".into(),
        geometry_column: Some("geometry".into()),
        ..Default::default()
    };
    let source = GeoParquetSource::open_http(
        config, "https://data.example.com/places-v1.parquet",
    ).await?;
    let result = source.get_tile(TileCoord { z: 0, x: 0, y: 0 }, None).await;
    source.close().await?;
    println!("{} bytes of MVT", result?.data.len());
    Ok(())
}
```

S3 入口使用相同源配置；由应用从自己的配置或凭据系统构造 `GeoParquetS3Options`。认证只保存在该源 DuckDB 实例的内存中，不安装 AWS 扩展、不读取环境凭据链，也不写入持久 secret。临时凭据过期后，应用使用新凭据重新打开并注册源。可运行的 HTTP/S3 接入示例见 [`geoparquet_remote`](../examples/geoparquet_remote.rs)。

HTTP 服务应支持合法的 Range 响应和对象校验标识。库禁用 DuckDB 的主动整文件下载及 Range 失败后的整文件下载回退，并保留 ETag 一致性检查。读取由 DuckDB 管理，若查询无法利用行组统计进行裁剪，累计读取仍可能覆盖整个文件。打开与每次查询受 `query_options.timeout_ms` 限制；在途网络请求的底层超时以 HTTPFS 的整秒超时为准。

应用必须授权远程地址及 S3 endpoint，并在部署网络层限制私网地址、云元数据地址、DNS 重绑定和重定向目标。HTTPFS 可能跟随 HTTP 重定向；本接口不再提供 Rust 客户端的 DNS 固定或自定义网络策略，不应直接接收未授权的用户 URL。库将 DuckDB 文件访问限制为指定输入，但这一限制不能代替网络出口隔离。

预签名 URL 和认证信息会进入该源的 DuckDB 会话。库关闭 HTTP 日志，并将远程后端错误转换为不含原始 URL、SQL 或响应内容的错误信息；数据源 ID 不应包含凭据。

### 从回环桥接接口迁移

`open_http` 保持原签名。`open_http_with_client` 已移除：普通/预签名 URL 改用 `open_http`，认证头改用 `open_http_with_headers`，客户端的 DNS、出口和重定向限制迁移至部署网络策略。`open_object_store` 已移除：S3 兼容存储改用 `open_s3`，其他存储需提供 HTTPFS 可访问的 HTTP(S) URL。任意 Rust `ObjectStore` 实例不会自动转换为 DuckDB 文件系统。

## 几何、属性与瓦片结果

动态切片接受 WKB 或可读取的 GEOMETRY 列。GeoParquet 元数据中的几何编码使用 `WKB`，边模型使用 `planar`。具体解析和查询逻辑见[几何查询实现](../src/sources/geoparquet_geometry.rs)。

### 坐标系与空间查询

`srid` 可以显式指定正整数 EPSG 代码。默认从 GeoParquet PROJJSON 读取 EPSG 或 OGC:CRS84 标识；省略 `crs` 时按 CRS84 处理，显式 `crs: null` 时由调用方提供 `srid`。对于显式指定几何列且省略 geo 元数据的 WKB Parquet，默认按 WGS84 处理。

查询使用 `always_xy` 按 x/y 顺序投影到 EPSG:3857，再完成空间相交、裁剪与 MVT 编码。有效坐标范围和缩放级别由配置控制；无效坐标或超出级别范围时返回 `TileNotFound`。

GeoParquet 1.1 的 `covering.bbox` 可加速行组裁剪。核心在 EPSG:4326、EPSG:3857 下使用可解析为数值字段的 bbox 路径；其他情况继续通过几何相交查询，读取范围可能覆盖全部行组。数据生产方负责保持每行 bbox 与几何一致，核心在打开时检查字段结构和类型。

TileJSON 优先使用显式 `bounds`，也可采用能够安全转换为 WGS84 的元数据范围。其余情况下保留空 bounds，以文件元数据完成打开过程。

### 属性与要素 ID

`properties = None` 自动选择 MVT 可表达的标量字段，`Some(...)` 使用显式白名单。常见映射如下：

| 输入类型 | MVT 属性表示 |
| --- | --- |
| 布尔值 | Boolean |
| 常规整数与浮点数 | Number |
| UInt64、128 位整数 | 十进制 String，保留整数精度 |
| 字符串、日期、时间、UUID、JSON、枚举 | String |
| DECIMAL | 浮点 Number |

容器、二进制及其他几何列由默认属性选择过程跳过；显式白名单中的字段需要具备 MVT 表示。几何列与 `id_column` 分别用于几何和要素 ID。

`id_column` 接受 8、16、32、64 位有符号或无符号整数列，其中非负值转换为 BIGINT 作为要素 ID，负值对应的要素省略 ID。ID 超出有符号 64 位范围时返回错误。查询命中零条要素或数据集为空时，几何源返回成功的空瓦片。

## 读取预编码瓦片归档

将 `geometry_column` 保持为 `None`，即可通过本地 `open` 读取 `(z, x, y, tile)` 归档。坐标列接受整数，瓦片列保存二进制字节。使用 `columns` 对接实际列名，并通过 `format`、`encoding` 描述已编码瓦片。

打开时选择这四列建立内存索引，随后按 XYZ 返回已保存的字节。归档准备要求来自 [`read_tile_archive`](../src/sources/geoparquet.rs)：

- 四个选定列使用无压缩 Parquet 列块，瓦片字节本身可采用 `encoding` 指定的压缩。
- 总扫描行数上限为 4,000,000，统计包含重复坐标与空值行。
- 单瓦片字节数遵守 `query_options.max_tile_bytes`。
- 索引与保留的瓦片字节使用 `memory_limit_mb * 1024 * 1024 / 2` 的预算，剩余预算用于读取和解码。
- 单行组所选列块的总量受总预算四分之一约束；元数据上限为总预算八分之一与 16 MiB 中的较小值。

超过读取预算时，打开操作返回错误。缺少指定坐标的归档瓦片时返回 `TileNotFound`。`tile_count()` 返回内存索引中的瓦片条目数。

## 查询预算

`GeoParquetSourceConfig.query_options` 控制每个几何源的执行资源，所有源克隆共享同一预算。默认值和有效范围来自 [`GeoParquetQueryOptions`](../src/sources/geoparquet_geometry.rs)。

| 字段 | 默认值 | 有效范围 | 作用 |
| --- | --- | --- | --- |
| `pool_size` | 2 | 1..=32 | 同时使用的 DuckDB 连接数 |
| `threads` | 2 | 1..=64 | DuckDB 工作线程数 |
| `memory_limit_mb` | 256 MB | 32..=65536 | DuckDB 管理的查询内存预算 |
| `timeout_ms` | 10000 ms | 1..=300000 | 单次瓦片请求排队与执行共用的期限 |
| `max_features` | 100000 | 1..=10000000 | 单瓦片命中要素数上限 |
| `max_tile_bytes` | 8 MiB | 1..=256 MiB | 单瓦片输出字节上限 |

接入 `SourceManager` 时，瓦片还需符合管理器的 16 MiB 单次响应载荷预算，见 [`manager.rs`](../src/sources/manager.rs)。要素数或输出大小超过相应预算时，查询返回错误。应用可以据此调整数据组织、请求级别或预算参数。DuckDB 查询内存由该源的连接共享，进程总 RSS 还包含运行时、读取缓冲和其他分配。

应用应同时限制活跃源数量，因为每个源各自持有 DuckDB 连接、文件描述符和远程网络资源。多源资源规划应同时考虑单源预算和源数量。

## 更新数据与关闭源

使用不可变、带版本的文件路径或对象 URI 管理数据。发布新版本时，先打开新数据源，再用相同 ID 注册到 `SourceManager`；注册过程开始关闭被替换的旧源。本地 GeoParquet 内容版本由源配置与文件标识计算；远程源每次打开使用新的内容代次，克隆共享同一代次，避免重新注册后复用旧会话的瓦片缓存。

本地几何查询检查文件大小和 mtime；文件变化后需要重新打开源。远程读取由 DuckDB HTTPFS 执行 ETag 一致性检查。应用应让每个版本地址始终对应同一内容，尤其是 HTTP 服务缺少版本校验标识时。热缓存命中可以直接返回已有结果，文件更新应通过明确的版本发布与重新注册流程完成。

关闭方式按资源管理目标选择：

| 操作 | 行为 |
| --- | --- |
| `source.start_close()` | 开始关闭所有共享会话的克隆，取消排队和在途查询 |
| `source.close().await` | 开始关闭并等待查询、DuckDB 连接和其网络资源退出 |
| `manager.close_source(id).await` | 移除源、使该源热缓存失效，并等待关闭完成 |
| `manager.remove(id)`、标记不可用、替换注册 | 开始关闭被移出的源，并立即返回 |

`close` 可以重复调用。若等待关闭的 future 被取消，可以通过保留的源再次 `await close()`。需要等待旧源完全退出时，在替换前保留旧源引用，再调用它的 `close`；`close_source` 操作的是管理器当前注册的源。

`with_file_lease` 可让源克隆和正在执行的阻塞查询持有调用方的文件生命周期守卫。即使请求被取消，守卫也会保留到对应阻塞查询退出。远程源关闭时等待在途 DuckDB 查询结束，然后释放连接和会话认证信息；保留源克隆不会保留已关闭会话的连接。若 `close()` 的等待被取消，可以再次调用并等待完成。

## 本地 FlatGeobuf 与一次性导入

启用 `local-vector` 并准备 Spatial 扩展后，可以从 `sources` 或 `prelude` 引入相关类型。FlatGeobuf 适合直接提供文件中的几何数据；GeoJSON 和 ZIP Shapefile 使用一次导入、多次瓦片查询的流程。

### 直接读取 FlatGeobuf

以下示例读取一个本地常规文件，并输出指定瓦片的 MVT 字节数：

```rust
use maptile_core::sources::{FlatGeobufSource, FlatGeobufSourceConfig, TileCoord, TileSource};

async fn read_flatgeobuf() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let source = tokio::task::spawn_blocking(|| {
        FlatGeobufSource::open(FlatGeobufSourceConfig {
            id: "places".into(),
            path: "/data/places.fgb".into(),
            layer_name: "places".into(),
            id_column: Some("id".into()),
            ..Default::default()
        })
    })
    .await??;
    let result = source.get_tile(TileCoord { z: 2, x: 2, y: 1 }, None).await;
    source.close().await?;
    println!("{} bytes of MVT", result?.data.len());
    Ok(())
}
```

输入使用单个本地文件的 UTF-8 路径，几何列由驱动发现。CRS 使用文件中可信的 EPSG 或 OGC:CRS84 标识，也可通过 `srid` 显式覆盖。数据投影到 EPSG:3857 后，按照带 buffer 的瓦片范围做精确相交和裁剪。

当前锁定的 Spatial 1.5.6 路径对有索引和无索引的 FlatGeobuf 都执行完整扫描。这一策略用于正确处理投影后的长线和空命中结果，相关回归见[本地矢量测试](../src/sources/local_vector.rs)。大文件的单瓦片扫描可能达到查询期限，应用应按数据体量安排预算。

FlatGeobuf 使用与 GeoParquet 几何源一致的连接池、文件变化检查、内容版本、查询预算和关闭流程。`FlatGeobufSourceConfig` 中的图层名、属性、ID、级别及裁剪字段按前述配置方式使用。

### 导入 GeoJSON 和 ZIP Shapefile

`import_to_geoparquet` 将本地输入转换为独立 GeoParquet 文件，并返回实际路径和几何列名。下例使用 ZIP Shapefile；GeoJSON 对应 `VectorImportFormat::GeoJson`：

```rust
use maptile_core::sources::{
    GeoParquetSource, GeoParquetSourceConfig, TileCoord, TileSource,
    VectorImportFormat, VectorImportOptions, import_to_geoparquet,
};

async fn import_places() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let imported = import_to_geoparquet(
        "/data/places.zip",
        VectorImportFormat::ShapefileZip,
        "/data/places.parquet",
        VectorImportOptions::default(),
    )
    .await?;
    let source = tokio::task::spawn_blocking(move || {
        GeoParquetSource::open(GeoParquetSourceConfig {
            id: "places".into(),
            path: imported.path,
            geometry_column: Some(imported.geometry_column),
            ..Default::default()
        })
    })
    .await??;
    let result = source.get_tile(TileCoord { z: 0, x: 0, y: 0 }, None).await;
    source.close().await?;
    println!("{} bytes of MVT", result?.data.len());
    Ok(())
}
```

导入将几何转换为 WGS84 WKB，保留 Z 维度并移除 M 维度。输出保留驱动读取的属性，生成 GeoParquet 1.1 `covering.bbox`；NULL 几何对应 NULL bbox。正常 GeoJSON 按 WGS84 处理，Shapefile 通过 `.prj` 识别 CRS。输入 CRS 需要显式确定时，设置 `VectorImportOptions.srid`。

输出几何列默认名为 `geometry`，与属性重名时自动追加下划线。因此，后续打开源时使用返回的 `geometry_column`。驱动解析或列名冲突会返回错误，例如输入属性 `wkb_geometry` 与 Spatial 生成的 WKB 列同名。

### 准备 Shapefile ZIP

导入器接受普通单卷 ZIP，组件可以位于嵌套目录中。准备文件时遵循以下输入契约：

| 项目 | 要求 |
| --- | --- |
| 图层数量 | 一套 Shapefile |
| 必需组件 | 同目录、同 stem 的 `.shp`、`.shx`、`.dbf`，匹配时忽略大小写 |
| 可选组件 | 同组 `.prj`、`.cpg`；提取过程使用上述五类组件 |
| 条目 | 1..=128 个，使用唯一的相对路径，内容为常规文件或目录 |
| 压缩 | Stored 或 Deflate，条目以明文存储 |
| 目录结构 | 32 位普通 ZIP 目录，中央目录最多 1 MiB，结束记录位于归档末尾 |
| 字节预算 | 声明的总解压体积与实际提取字节分别受 `max_bytes` 约束 |

导入器在建立完整条目索引前校验中央目录，并检查路径、文件类型、组件重复和归档头一致性。提取后还会核对 `.shp/.shx` 文件头长度、索引记录数及 `.dbf` 记录体积；几何记录由 Spatial 内置 GDAL 驱动解析。

### 导入预算与文件发布

导入配置由 [`VectorImportOptions`](../src/sources/local_vector.rs) 定义：

| 字段 | 默认值 | 有效范围与用途 |
| --- | --- | --- |
| `srid` | `None` | 用正整数 EPSG 代码覆盖输入 CRS |
| `max_bytes` | 512 MiB | `1..=8 GiB`，分别限制输入、ZIP 声明解压总量、实际提取量和完成后的输出 |
| `timeout_ms` | 30000 ms | `1..=300000`，暂存、等待工作线程与转换共用的期限 |

每次导入使用 2 个 DuckDB 线程和 256 MB DuckDB 管理内存，并将磁盘 spill 预算设为零。输出大小在 `COPY` 完成后检查，应用另外配置并行导入数量与磁盘配额。

调用方授权输入文件和目标目录，并选择一个尚未创建的输出路径。转换在目标父目录内的私有临时目录进行，Unix 目录权限为 `0700`，发布文件权限遵循进程 umask。完整转换成功后，导入器通过同一文件系统的硬链接原子发布结果，因此目标文件系统需要支持硬链接。

失败或取消会清理暂存结果，清理时间以阻塞任务退出为准。成功返回的 GeoParquet 由调用方持有，可独立于原始 GeoJSON 或 ZIP 使用；应用可在确认导入成功后清理原始输入。直接提供瓦片的 FlatGeobuf 文件应在源的使用期间保持内容不变。

## 运行示例与验证

仓库中的 [`local_vector` 示例](../examples/local_vector.rs)读取世界瓦片 `0/0/0` 并打印 MVT 字节数。准备好对应数据文件，在仓库根目录运行：

```sh
cargo run --locked --features local-vector --example local_vector -- fgb places.fgb
cargo run --locked --features local-vector --example local_vector -- geojson places.geojson places-json.parquet
cargo run --locked --features local-vector --example local_vector -- shapefile places.zip places-shp.parquet
```

导入示例中的输出路径应为新路径。使用已有数据进行复测时，可以直接打开已生成的 GeoParquet。

扩展准备完成后，按需要验证 GeoParquet 或包含本地矢量能力的组合：

```sh
cargo test --locked --features geoparquet --lib
cargo clippy --locked --all-targets --features geoparquet -- -D warnings
cargo test --locked --features local-vector --lib
cargo clippy --locked --all-targets --features local-vector -- -D warnings
```

[几何查询测试](../src/sources/geoparquet_geometry.rs)和[本地矢量测试](../src/sources/local_vector.rs)使用合成文件覆盖投影、属性与 ID、跨边界几何、covering、查询预算、HTTP Range、源管理、导入和关闭流程。[`geoparquet` CI 作业](../.github/workflows/ci.yml)启用 `local-vector` 运行这组能力，FGB 与 Shapefile 测试数据由 Spatial 生成。部署验收可再结合实际对象存储端点、应用数据源创建流程、目标平台和业务负载执行集成验证。

查看重复请求的本地微基准可运行：

```sh
cargo test --locked --features geoparquet --lib repeated_tile_probe -- --ignored --nocapture
```

该命令用于观察合成数据下的重复查询开销；容量规划应采用实际数据和并发负载的测量结果。
