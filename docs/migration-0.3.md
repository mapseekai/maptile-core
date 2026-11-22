# 从 0.3 迁移

迁移 0.3 的应用通过 `CatalogService` 发布版本化目录，通过 `TileStack` 读取瓦片。先在测试环境使用实际数据比对瓦片、图层、属性、样式和结果编码。

## 选择后端

默认 features 仍为空，但后端依赖现在也由 feature 控制。使用归档或数据库的应用需要在 Cargo 配置中显式添加 `pmtiles`、`mbtiles` 或 `postgres`。`cloud` PMTiles 还需要 `pmtiles`。COG 和固定镶嵌使用 `agtiff`，STAC 使用 `stac`，PostGIS OutDB 同时需要 `postgres,raster`。完整列表见[功能开关](features.md)。

本地 FlatGeobuf 和 GeoParquet 使用 DuckDB Spatial。预编码 Parquet 瓦片使用独立的 `parquet-archive`。在线 GeoJSON 和 ZIP Shapefile 不再作为矢量切片或导入入口；先将数据转换为 GeoParquet 或 FlatGeobuf。准备匹配版本的扩展和数据，见[矢量文件指南](geoparquet.md)。

## 替换旧接口

| 0.3 的旧接口 | 迁移后的调用 |
| --- | --- |
| `SourceManager::with_cache`、`register`、`get_tile` | 用 `CatalogService::new(cache)` 构造服务，通过 `catalog.catalog().update()` 的 `put` 和目录的 `publish` 发布数据源。用 `TileStack::read` 读取。 |
| `SourceManager::get`、`snapshot_tilejson` | 从 `SourceCatalog::snapshot()` 取得当前代次，用 `describe(id)` 或 `descriptors()` 读取元数据。 |
| `SourceManager::snapshot_catalog`、`Catalog` | 用 `CatalogSnapshot::descriptors()` 获取源目录，用 `styles::snapshot` 获取样式列表。需要稳定的源列表顺序时，由应用按源 ID 排序。 |
| `SourceManager::reload_from_discovery`、`ReloadDelta` | 用 `SourceCatalog::reconcile` 提交带预期代次的发现清单。 |
| `close_source`、`mark_unavailable_if_current`、`recover_unavailable` | 常规下线使用 `CatalogUpdate::remove` 和 `publish`。紧急撤销使用操作实际持有的快照调用 `revoke`。恢复时重新打开源并发布。 |
| `CompositeSource`、`LazyCompositeSource`、`sources::mvt::merge` | 在一个目录更新中发布 `CompositeDeclaration`，按需设置 `lazy` 与重复图层策略。 |
| `parse_composite_id_list` | 将逗号分隔的成员 ID 交给 `ReadTarget`。空成员会返回 `InvalidRequest`，旧 URL 如 `roads,,water` 应改为 `roads,water`。 |
| `TileSource` 的默认 `content_version`、`clone_source` | 返回打开时固定的数据修订。同一目录再次发布该实例时用 `Arc::clone` 复用原句柄。替换时独立打开源，旧实例退役会关闭其资源。 |
| `get_tile_with_etag`、`TileEtagResponse` | 用 `TileStack::read` 获取瓦片，在 HTTP 适配器中选择表示、计算强 ETag 并处理 `If-None-Match`。 |
| `rewrite_style_for_native` 的管理器参数 | 将 `&CatalogSnapshot` 传入重写函数。 |
| `WmtsService`、`WmtsLayer`、`build_capabilities` | 在应用中生成能力 XML。可参考 [HTTP 示例中的 WMTS 模块](../examples/archive_http/wmts.rs)。 |

## 迁移读取和组合

如果工厂统一返回多种后端，将返回类型从 `Arc<dyn TileSource>` 改为 `Arc<dyn TileDriver>`，在各分支打开具体源时完成类型转换。将该句柄交给 `CatalogUpdate::put`，再用 `SourceCatalog::publish` 发布。通过 `TileStack::builder(catalog).build()` 读取。

将调用方自定义操作注册为 `TileLayer`。固定可见范围、鉴权和 HTTP 条件请求留在应用。每个授权范围使用不同的 `ReadTarget::scope`，防止共用缓存跨范围返回数据。

为应用逐请求 Raster Style 的自定义驱动声明 `SourceCapabilities::raster_style`，或在 `TileSource` 上实现 `supports_raster_style()`。调用方只向具备该能力的源发送 `style` 和 `style_hash`。归档读取沿用归档保存的表示。

处理 `TileOutcome::NoTile`、`Present` 和 `Partial`。合法零字节瓦片与无瓦片不同。部分结果禁止写入规范缓存，HTTP 层发送 `Cache-Control: no-store`，不生成强 ETag，也不因 `If-None-Match` 返回 304。

按原成员顺序配置 `CompositeDeclaration`。依赖后层覆盖的应用可使用 `DuplicateLayer::KeepLast` 并比对结果。原来使用 `layer_prefix` 的应用需要先调整生产端图层名和样式的 `source-layer` 引用。使用按逗号分隔的临时组合前，检查各成员的图层声明和查询能力。 [组合准入与策略](sources.md#组合多个-mvt-源)

临时组合 ID 现在按成员顺序拼接各自去掉首尾空白的 ID。旧版按带空白的组合 ID 生成的无状态导出游标需要从第一页重新开始。

## 发布版本和处理 HTTP

以不可变文件、版本化对象地址或明确的数据库修订建立新源。自定义 `TileSource` 缺少权威版本时，在打开时调用 `ContentVersion::new_generation()`，随后从 `content_version()` 返回保存的值。准备和校验全部对象后发布新目录。发现扫描仅在清单完整时使用 `Inventory::Complete`，部分扫描使用 `Partial`，失败时保留旧目录。 [数据源版本与接入](sources.md#自定义数据源与内容版本)

服务端根据最终格式、编码和响应字节生成强 ETag，并设置相应 `Vary`。对外 TileJSON 必须包含绝对瓦片地址，矢量层包含 `id` 和 `fields`。使用应用配置的可信公开地址生成 WMTS XML。可运行只绑定环回地址的 HTTP 示例来验证这些调用。 [HTTP 示例](../examples/archive_http.rs) · [启动命令](getting-started.md#5-处理瓦片结果)

切换前验证冷缓存、热缓存、导出分页、条件请求、权限范围、远程对象换版及业务数据的显示结果。在验收完成前保留旧服务的构建、配置和数据版本。验收失败时，可切回旧服务继续处理请求。旧代请求和导出结束后再回收对应的资源。
