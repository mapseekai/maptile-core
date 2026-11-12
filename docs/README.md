# maptile-core 文档

这些文档面向将地图瓦片、矢量数据处理或地图图片渲染接入应用的 Rust 开发者。首次接入从[读取第一张瓦片](getting-started.md)开始；选择数据格式或构建后端时，查阅[功能开关](features.md)。

## 入门教程

[读取第一张瓦片](getting-started.md)使用仓库自带的 PMTiles 合成样例，完成打开文件、注册数据源、启用缓存、读取结果和关闭数据源。完成后，可以将同一流程用于自己的归档。

## 操作指南

| 目标 | 阅读文档 | 可完成的操作 |
| --- | --- | --- |
| 发布归档或数据库中的瓦片 | [数据源与查询](sources.md) | 接入 PMTiles、MBTiles 和 PostGIS，配置过滤，组合图层，更新数据源 |
| 使用矢量文件 | [GeoParquet 与本地矢量](geoparquet.md) | 准备 DuckDB 扩展，接入本地或远程 GeoParquet，读取 FlatGeobuf，导入 GeoJSON 与 Shapefile |
| 生成地图或影像图片 | [样式与渲染](rendering.md) | 加载样式，构造渲染请求，配置 COG 与 STAC，生成 WMTS 元数据 |
| 开发和验证本库 | [开发与构建](development.md) | 准备工具链，运行检查，配置数据库测试，构建原生渲染后端 |

## 技术参考

[功能开关](features.md)列出默认能力、全部 Cargo features 和主要依赖。类型、字段和方法的详细参考通过 Rustdoc 生成：

```sh
cargo doc --locked --no-deps --open
```

为相应后端生成 API 文档时，在命令中加入 `--features`，例如 `--features local-vector`。从 [`prelude`](../src/prelude.rs) 可以查看常用类型的导出路径。

## 原理说明

[架构与生命周期](architecture.md)说明应用如何组合数据源、缓存和样式，以及内容版本、缓存键、文件变更和关闭操作之间的关系。

公共接口的实现入口包括 [`TileSource`](../src/sources/mod.rs)、[`SourceManager`](../src/sources/manager.rs)、[`TileCache`](../src/cache/tile_cache.rs) 和 [`render`](../src/render/mod.rs)。每篇专题文档链接到其对应的实现，便于核对配置与行为。

## 项目记录

- [历史变更记录](CHANGELOG.md)：版本变化与调用方迁移信息。
- [独立项目来源](EXTRACTION.md)：提取来源、时间和独立化记录。
- [第三方声明](third-party-notices.md)：代码归属、许可证和分发要求。

[返回项目首页](../README.md)
