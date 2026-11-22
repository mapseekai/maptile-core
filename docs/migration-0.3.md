# 从 0.3 迁移

先在测试环境使用实际数据迁移并比对瓦片、图层、属性、样式和结果编码。旧 `SourceManager`、`TileSource` 和 `get_tile_with_etag` 仍可用；新接入使用 `TileStack` 和版本化目录。两套读取路径各自维护生命周期与缓存。

## 选择后端

默认 features 仍为空，但后端依赖现在也由 feature 控制。使用归档或数据库的应用需要在 Cargo 配置中显式添加 `pmtiles`、`mbtiles` 或 `postgres`。`cloud` PMTiles 还需要 `pmtiles`。COG 和固定镶嵌使用 `agtiff`，STAC 使用 `stac`，PostGIS OutDB 同时需要 `postgres,raster`。完整列表见[功能开关](features.md)。

本地 FlatGeobuf 和 GeoParquet 使用 DuckDB Spatial。预编码 Parquet 瓦片使用独立的 `parquet-archive`。在线 GeoJSON 和 ZIP Shapefile 不再作为矢量切片或导入入口；先将数据转换为 GeoParquet 或 FlatGeobuf。准备匹配版本的扩展和数据，见[矢量文件指南](geoparquet.md)。

## 迁移读取和组合

打开数据源后，用 `CatalogService` 的更新对象 `put` 并 `publish`，通过 `TileStack::builder(catalog).build()` 读取。将调用方自定义操作注册为 `TileLayer`。固定可见范围、鉴权和 HTTP 条件请求留在应用。每个授权范围使用不同的 `ReadTarget::scope`，防止共用缓存跨范围返回数据。

处理 `TileOutcome::NoTile`、`Present` 和 `Partial`。合法零字节瓦片与无瓦片不同。部分结果禁止写入规范缓存，HTTP 层发送 `Cache-Control: no-store`，不生成强 ETag，也不因 `If-None-Match` 返回 304。

将旧组合声明改为 `CompositeDeclaration`。严格组合要求每个成员保证 MVT 合规并完整声明层名。普通外部归档仍可独立读取，生产端能保证所有坐标的 MVT 合规并完整声明层名时，应用才可通过 `put_trusted_mvt` 显式授予严格组合准入。默认 `DuplicateLayer::Error` 拒绝重名；原来依赖后层覆盖时使用 `KeepLast` 并比对结果。严格组合不在运行时添加 `layer_prefix`，先调整生产端图层名和样式的 `source-layer` 引用。检查原有按逗号动态组合的调用和所有成员的过滤能力。

## 发布版本和处理 HTTP

以不可变文件、版本化对象地址或明确的数据库修订建立新源。数据或影响响应元信息的配置发生变化时更新修订。准备和校验全部对象后发布新目录，旧快照持有者退出后再回收旧资源。远程多次 Range 读取必须固定同一对象版本。发现扫描仅在清单完整时使用 `Inventory::Complete`，部分扫描使用 `Partial`，失败时保留旧目录。

服务端根据最终格式和编码生成强 ETag，并设置相应 `Vary`。对外 TileJSON 必须包含绝对瓦片地址，矢量层包含 `id` 和 `fields`。WMTS XML 的 URL 必须由可信的公开地址生成。不要将客户端提供的 Host 当作可信地址。可运行[HTTP 示例](../examples/archive_http.rs)验证接法；该示例只绑定环回地址，不包含鉴权。

切换前分别验证冷缓存、热缓存、导出分页、条件请求、权限范围、远程对象换版及生产数据的显示结果。保留旧资源直到旧代请求完成。
