# 功能开关

`maptile-core` 默认提供 `TileStack`、目录、缓存、CQL2、MVT 组合和通用瓦片类型。默认普通依赖排除 DuckDB、SQLite、PostgreSQL 驱动、PROJ、GDAL、原生 zstd 和 MapLibre Native。具体声明见 [Cargo.toml](../Cargo.toml)。

| Feature | 能力 | 依赖与配置 |
| --- | --- | --- |
| `pmtiles` | 本地和 HTTP PMTiles 读取及归档写入 | 启用 `zstd` 后可使用 Zstandard |
| `mbtiles` | 只读 MBTiles 源及归档维护工具 | SQLite 与 Martin 的 MBTiles 实现 |
| `postgres` | PostGIS 表和 PostgreSQL 瓦片函数 | 使用 PostgreSQL 客户端及连接池 |
| `cloud` | 对象存储访问 | 同时使用 `pmtiles` 启用 PMTiles 对象存储读取，由应用注入客户端 |
| `duckdb` | GeoParquet 和本地 FlatGeobuf 动态切片 | 使用 bundled DuckDB 和 Spatial。直接读取不可变 HTTP/S3 对象需要 HTTPFS；强 ETag 的 HTTP 读取先验证分段并固定本地快照 |
| `geoparquet`、`local-vector` | 启用 `duckdb` 的兼容名称 | 参见 [GeoParquet 与 FlatGeobuf](geoparquet.md) |
| `parquet-archive` | 读取预编码瓦片的 Parquet 归档 | 使用 Arrow 和 Parquet，归档瓦片列块使用未压缩编码 |
| `agtiff` | COG 切片、像元点查和多景镶嵌 | 使用 async-geotiff 与 PROJ |
| `stac` | STAC 搜索和 Raster Style 镶嵌 | 启用 `agtiff` |
| `raster` | 通过 GDAL 导出原生 GeoTIFF 覆盖 | 启用 `agtiff`。PostGIS Out-DB 同时启用 `postgres` |
| `zstd` | Zstandard 瓦片传输编解码 | 启用原生 zstd |
| `mlt` | MVT 与 MLT 有界转换 | 拒绝无法保留的内容，如未知 MLT 帧和混合属性类型的 MVT 字段 |
| `metrics` | Prometheus 缓存请求计数器 | 应用抓取默认 registry |
| `render` | MapLibre Native 渲染器及覆盖物叠加 | macOS 使用 Metal，Linux 默认使用 Vulkan |
| `render-opengl` | Linux OpenGL 渲染后端 | 启用 `render` |
| `render-bench` | 原生渲染生命周期测量 | 启用 `render` |

## 常用组合

读取本地 PMTiles 并验证示例中的瓦片。

```sh
cargo run --locked --features pmtiles --example read_pmtiles
```

从 S3 兼容对象存储读取 PMTiles 时，同时启用 `pmtiles,cloud`。`cloud` 可以与所需的编解码开关组合。

只需动态矢量切片时启用 `duckdb`。只需读取预编码的 Parquet 瓦片时启用 `parquet-archive`。匹配扩展的安装方法见 [GeoParquet 与 FlatGeobuf](geoparquet.md)。

启用 `agtiff` 可读取 COG 和固定资产列表。固定镶嵌按资产配置顺序读取，`First` 在有效像元填满后停止。NoData 可由后续资产填补。全部资产在某个像元都没有有效值时，该像元透明。固定镶嵌中的成员应使用不可变地址与版本标识，更新时发布新的成员配置。

`StacSource::open` 每次读取重新查询资产。`open_with_search_cache` 可在指定容量和 TTL 内复用顺序固定的搜索结果。动态 STAC 的渲染结果绕过共享瓦片缓存。覆盖导出需要 `raster`。Out-DB 可通过 `with_dataset_cache` 配置 GDAL 数据集缓存的容量和 TTL。

原生地图渲染的依赖准备见[开发与构建](development.md#原生渲染构建)。

## 验证

[CI](../.github/workflows/ci.yml)分别构建与测试最小核心、各归档后端、PostGIS、GeoParquet、COG、STAC、GDAL 覆盖及可选编解码。在 Linux 和 macOS 上检查最小依赖树和链接产物，也检查 `agtiff` 与 `stac` 的 GDAL 隔离。

```sh
bash scripts/ci/check-feature-dependencies.sh ""
bash scripts/ci/check-feature-dependencies.sh parquet-archive
bash scripts/ci/check-feature-dependencies.sh agtiff
```
