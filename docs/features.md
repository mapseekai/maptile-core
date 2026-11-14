# 功能开关

`maptile-core` 使用 Cargo features 组合数据后端、图像处理和原生渲染能力。默认 feature 列表为空；默认构建已经包含常用瓦片归档、PostgreSQL、缓存和样式组件。完整声明以 [Cargo.toml](../Cargo.toml) 为准，公共模块入口见 [`src/lib.rs`](../src/lib.rs)。

## 默认构建

| 能力 | 主要入口 | 典型用途 |
| --- | --- | --- |
| PMTiles v3 | `LocalPmtilesSource`、`HttpPmtilesSource` | 本地与 HTTP Range 归档读取 |
| MBTiles | `MbtilesSource`、`mbtiles::tools` | 读取归档，使用复制、补丁、验证和元数据工具 |
| PostgreSQL / PostGIS | `PostgresPool`、`PostgresTableSource`、`PostgresFunctionSource` | 动态 MVT 与数据库瓦片函数 |
| MVT 组合 | `CompositeSource`、`LazyCompositeSource` | 合并图层，按源 ID 组织组合数据源 |
| 缓存和元数据 | `SourceManager`、`TileCache`、TileJSON、ETag | 数据源注册、共享缓存、目录快照 |
| 属性过滤 | `filter` | 解析 CQL2 文本或 JSON，按列白名单验证并生成 SQL 条件 |
| 样式与栅格类型 | `styles`、`raster` | 样式存储、URL 重写、Raster Style 文档与重采样类型 |
| 渲染请求与服务元数据 | `RenderRequest`、`StaticTarget`、`build_capabilities` | 构造地图图片请求，生成 WMTS XML |
| 编码和传输 | `wkb_mvt`、`transport` | WKB 到 MVT，gzip/zlib/brotli/zstd 编解码 |
| 文件发现 | `discovery`、`watch` | 扫描源和样式文件，接收变更事件 |

每种数据源消费的查询参数由其实现决定。PostGIS 属性过滤的配置方式见[数据源与查询](sources.md#属性与空间范围查询)。

## 可选 features

| Feature | 启用的能力 | 依赖与配置要点 |
| --- | --- | --- |
| `cloud` | 对象存储中的 PMTiles | 使用 `object_store`，由应用注入存储客户端 |
| `duckdb` | DuckDB SQL 瓦片源；预编码 BLOB 或 WGS84 WKB 动态切片 | 编译 bundled DuckDB，准备 C/C++ 工具链 |
| `geoparquet` | GeoParquet 瓦片归档、本地及远程几何动态切片 | Arrow/Parquet 与 bundled DuckDB；Spatial 执行几何查询，HTTPFS 直接读取 HTTP/S3，无本机 HTTP 桥 |
| `local-vector` | FlatGeobuf 直接切片，GeoJSON 与 ZIP Shapefile 导入 GeoParquet | 包含 `geoparquet`，使用 DuckDB Spatial 中的矢量驱动 |
| `agtiff` | async-geotiff COG 切片、栅格样式和原始像元查询 | 使用原生 PROJ 进行坐标变换 |
| `raster` | GDAL 栅格处理、覆盖导出、镶嵌源和 PostGIS OutDB | 包含 `agtiff`，准备 GDAL 与 PROJ |
| `stac` | STAC 资产发现与 Raster Style 镶嵌渲染 | 包含 `agtiff`，配置 STAC 地址与资产读取环境 |
| `mlt` | MVT 与 MLT 转换 | 启用 `mlt-core` 和几何转换依赖 |
| `metrics` | Prometheus 缓存请求计数器 | 首次访问时注册默认 Prometheus registry，应用抓取并暴露指标 |
| `render` | MapLibre Native 渲染器、渲染池、图片编码和覆盖物叠加 | 固定 `maplibre_native =0.9.3`；macOS 使用 Metal，Linux 默认使用 Vulkan |
| `render-opengl` | Linux OpenGL 渲染后端 | 包含 `render`，使用 Linux 对应图形与构建环境 |
| `render-bench` | 原生渲染生命周期基准与测量接口 | 包含 `render`，用于 `render_native` 基准 |

`geoparquet` 直接启用其所需的 DuckDB 依赖。应用需要公开的 `DuckDbSource` 时，再选择 `duckdb` feature。

## 常用组合

### GeoParquet 与本地矢量

```toml
[dependencies]
maptile-core = { git = "https://github.com/mapseekai/maptile-core.git", features = ["local-vector"] }
```

这一组合同时提供 GeoParquet 与本地矢量入口。安装匹配的 DuckDB 扩展后，按照[矢量文件指南](geoparquet.md)准备源配置。

### COG 与 STAC

```toml
[dependencies]
maptile-core = { git = "https://github.com/mapseekai/maptile-core.git", features = ["stac"] }
```

`stac` 已包含 `agtiff`。使用 GDAL 覆盖导出、`MosaicSource` 或 PostGIS OutDB 时，加上 `raster`。

### 原生地图渲染与指标

```toml
[dependencies]
maptile-core = { git = "https://github.com/mapseekai/maptile-core.git", features = ["render", "metrics"] }
```

为目标平台选择图形后端，并按[原生渲染构建](development.md#原生渲染构建)准备 MapLibre Native。Linux OpenGL 构建选择 `render-opengl`。

## 构建与验证入口

本仓库的 [CI](../.github/workflows/ci.yml)按默认、`cloud,metrics,mlt`、`local-vector` 三组组织测试。GDAL/PROJ 与原生图形后端使用各自的环境和验证命令，见[开发与构建](development.md)。

按实际部署组合构建和检查，可让验证结果直接对应应用启用的能力。
