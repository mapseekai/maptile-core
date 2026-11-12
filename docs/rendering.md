# 地图与栅格渲染

`maptile-core` 提供两条图像生成路径：MapLibre Native 按地图样式合成矢量、栅格和标注图层；COG、STAC 与 OutDB 数据源按 Raster Style 处理影像并生成瓦片。应用可以按数据和输出目标组合这些组件。

公共入口见 [`render`](../src/render/mod.rs)、[`styles`](../src/styles/mod.rs) 和 [`raster::style`](../src/raster/style/mod.rs)。本页面向负责接入样式、图片渲染和影像瓦片的 Rust 开发者。

## 选择渲染入口

| 目标 | 主要接口 | Feature |
| --- | --- | --- |
| 读取样式、调整资源 URL | `LocalStyleStore`、`StyleStore`、`rewrite_urls` | 默认构建 |
| 构造地图请求、解析视口 | `RenderRequest`、`RenderCamera`、`StaticTarget` | 默认构建 |
| 生成 WMTS 服务描述 | `WmtsService`、`WmtsLayer`、`build_capabilities` | 默认构建 |
| 按地图样式输出图片 | `RendererPool`、`MapLibreRenderer` | `render` |
| 单幅 COG 动态切片 | `AsyncCogSource` | `agtiff` |
| 按目录搜索并镶嵌 COG | `StacSource` | `stac` |
| 固定成员镶嵌、PostGIS OutDB | `MosaicSource`、`PostgresOutDbRasterSource` | `raster` |

默认构建可完成样式管理、请求构造和 WMTS XML 生成。执行地图图片渲染时启用 `render`，由工作线程持有原生渲染器。组合方式见[功能开关](features.md)，平台依赖和构建命令见[原生渲染构建](development.md#原生渲染构建)。

## 从样式生成地图图片

在仓库根目录将下例保存为 `examples/render_map.rs`。代码加载 [`testdata/render-style.json`](../testdata/render-style.json)，创建渲染池并写出 `render.png`。样式包含深色背景和内嵌 GeoJSON 橙色点，可用于验证完整图片生成过程。

```rust
use maptile_core::render::{
    PoolConfig, RenderFormat, RenderRequest, RendererPool,
};
use maptile_core::styles::{LocalStyleStore, StyleStore};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let store = LocalStyleStore::new([(
        "preview",
        PathBuf::from("testdata/render-style.json"),
    )])?;
    let style = store.get("preview").await?;
    let pool = RendererPool::new(PoolConfig {
        max_concurrent: 2,
        ..PoolConfig::default()
    })?;

    let request = RenderRequest::for_tile(
        style.value.to_string(),
        0,
        0,
        0,
        1,
        RenderFormat::Png,
    );
    let image = pool.render(request).await?;
    tokio::fs::write("render.png", &image.bytes).await?;
    println!("{} x {} {}", image.width, image.height, image.format.content_type());
    Ok(())
}
```

在 `Cargo.toml` 中为这个示例声明所需 feature，让 Cargo 按渲染构建组合选择它：

```toml
[[example]]
name = "render_map"
required-features = ["render"]
```

准备好原生构建环境后，在仓库根目录执行：

```sh
bash scripts/ci/maptile-render-cargo.sh run --locked --features render --example render_map
```

示例输出 `512 x 512 image/png`。正式接入时，在应用生命周期内复用渲染池，并将样式的数据源、字体和精灵图地址配置为渲染进程可访问的资源。示例调用对应 [`RendererPool`](../src/render/pool.rs) 与[原生渲染集成用例](../tests/render/native.rs)。

## 样式与资源地址

`LocalStyleStore::new` 接收样式 ID 到 JSON 文件路径的映射；`get(id).await` 读取并解析文件，`list().await` 返回排序后的样式 ID。返回的 `Style` 包含 `id`、`name`、JSON 值和源文件路径。应用可通过 `StyleStore` 接口提供自己的存储实现。

`rewrite_urls(&style, base_url, query_string)` 将以下字段中以 `/` 开头的资源地址转换为绝对 URL：

- `sources[*].url` 与 `sources[*].tiles[*]`。
- `glyphs` 和字符串形式的 `sprite`。

`query_string` 按原样追加到这些 URL 的末尾。传入空字符串可只补齐域名；需要附加参数时，由应用准备包含正确分隔符的后缀。已有完整 URL 保持其配置值。

`rewrite_style_for_native(&style, base_url, &manager)` 处理指向已注册源的 `/data/<id>` 或 `/data/<id>.json` 引用：将其转换为瓦片 URL 模板，并从源元数据补充缺省的缩放范围、边界和署名。应用提供对应瓦片、字体和精灵图的资源访问路径。具体重写规则见 [`styles/store.rs`](../src/styles/store.rs)，源注册见[数据源与查询](sources.md)。

## 配置相机与覆盖物

按 XYZ 输出地图瓦片时，`RenderRequest::for_tile` 根据瓦片中心构造相机。静态图片可直接设置 `RenderCamera`，也可以先将字符串解析为 `StaticTarget`，再调用 `render::static_map::build_request`。

| 输入示例 | 视口含义 | 调用方式 |
| --- | --- | --- |
| `116.4,39.9,12` | 经度、纬度、缩放级别 | 解析为 `StaticTarget::Center` |
| `116.4,39.9,12@45,30` | 中心、缩放、方位角和俯仰角 | 解析为 `StaticTarget::Center` |
| `116.2,39.7,116.6,40.1` | 西、南、东、北边界 | 解析为 `StaticTarget::BoundingBox` |
| `auto` | 按应用提供的覆盖物适配视口 | 应用先计算边界，再构造边界视口 |

中心视口保留传入的相机参数。边界视口按 Web Mercator 和图片宽高估算缩放级别，计算结果落在 0 到 22。相机参数、输入语法和计算过程见 [`static_map.rs`](../src/render/static_map.rs)。

自动适配覆盖物时，按以下顺序组织调用：

1. 使用 `expand_overlays` 将覆盖物展开为路径和标记。
2. 使用 `calculate_bounds(&paths, &markers)` 得到地理边界；应用根据结果添加留白或选择默认视口。
3. 用 `StaticTarget::BoundingBox` 构造请求，再将原覆盖物放入 `request.overlay_items`。

`render` feature 提供路径、标记和 GeoJSON 覆盖物，可配置颜色、线宽、填充与标记尺寸。渲染器先生成地图图像，再绘制覆盖物并编码。公开类型和解析函数见 [`overlay.rs`](../src/render/overlay.rs)。

## 输出尺寸与并发

`RenderFormat` 提供 PNG、JPEG 和 WebP。返回的 `RenderedImage` 包含编码字节、格式和实际像素尺寸；`content_type()` 返回对应 MIME 类型。

| 配置 | 当前值或要求 | 使用方式 |
| --- | --- | --- |
| `for_tile` 逻辑尺寸 | 512 × 512 | 适合按 XYZ 渲染地图瓦片 |
| `width`、`height` | 均大于 0 | 静态图片按目标宽高设置 |
| `scale` | 整数 1 到 4 | 输出尺寸为逻辑宽高乘以 `scale` |
| 单边输出尺寸 | 最大 4096 像素 | 分别校验 `width × scale` 和 `height × scale` |
| 相机参数 | 有限数值 | 提交前通过 `RenderRequest::validate` 检查 |

尺寸与格式规则以 [`render/types.rs`](../src/render/types.rs) 为准。`RendererPool` 在提交任务时执行请求校验。

| `PoolConfig` 字段 | 默认值 | 含义 |
| --- | --- | --- |
| `max_concurrent` | 4 | 持久渲染线程数量；传 0 时按 1 个线程处理 |
| `default_size` | `(512, 512)` | 工作线程创建渲染器时的初始画布尺寸 |
| `queue_depth_per_worker` | 4 | 每个线程对应的队列容量份额，设置为正整数 |

队列总容量为实际线程数乘以 `queue_depth_per_worker`。`render(request)` 使用 100 毫秒的入队等待期限；`try_render_with_timeout(request, duration)` 可设置这一期限。该参数计量队列接收任务所用的等待时间，完整响应时间还包括任务在队列中的等待和原生渲染执行。定义与实现见 [`render/pool.rs`](../src/render/pool.rs)。

应用需要为某次请求创建独立原生上下文时，可以设置 `request.isolate_native_state = true`。这一选项由可信应用代码决定，适合为请求配置独立的资源访问上下文，字段语义见 [`RenderRequest`](../src/render/types.rs)。

## COG 栅格切片

启用 `agtiff` 后，`AsyncCogSource::open` 接收 `AsyncCogSourceConfig`。其中 `CogObjectSource` 提供本地绝对路径、HTTP(S) URL、已配置的 `Arc<dyn ObjectStore>` 与对象键三种入口。

每次切片请求通过 `TileQuery.style` 提供 `EffectiveStyle`。应用先用 `RasterStyle::from_json` 或 `from_value` 解析 Raster Style 文档，再通过 `EffectiveStyle::from_raster_style` 取得可用于查询和缓存的样式。选择波段、渲染器、拉伸、色带、重采样和输出参数时，以所用数据和样式声明为准。

`EffectiveStyle` 保存样式文档、瓦片尺寸、输出格式和样式散列；COG 适配器将其传给 async-geotiff，并在 `TileResponse.info` 中记录实际 PNG、WebP 或 JPEG 格式。实现见 [`style/resolve.rs`](../src/raster/style/resolve.rs)、[`agtiff_style.rs`](../src/raster/agtiff_style.rs) 与 [`agtiff_cog.rs`](../src/sources/agtiff_cog.rs)。

配置 COG 时，同时准备以下资源：

- 为 `content_id` 提供非空的数据版本标识，例如版本化 URI 或内容指纹。
- 通过 `new_shared_block_cache()` 创建共享缓存，将同一个 `Arc<BlockCache>` 注入各源的 `block_cache`。
- 为坐标变换准备匹配的 PROJ 库及数据，具体环境见[开发与构建](development.md)。
- 更新影像时，使用新的内容标识重新打开并注册源，使块缓存和瓦片缓存按数据版本分开。

将 `block_cache` 设置为 `None` 时，源使用独立的默认缓存。应用可根据活跃源数量和读取模式选择内存预算；源生命周期与缓存关系见[架构与调用流程](architecture.md)。

## STAC、固定镶嵌与 OutDB

下列入口共同接受 Raster Style，通过各自的数据访问方式组织影像：

| 入口 | 影像组织方式 | 接入要点 |
| --- | --- | --- |
| `StacSource` | 按瓦片范围搜索 STAC Item，并通过 async-geotiff 镶嵌 COG 资产 | 配置 API、集合、时间条件和资产角色；用 `list_assets_for_tile` 查看候选资产 |
| `MosaicSource` | 应用提供固定的 `MosaicMember` 列表，使用 async-geotiff 镶嵌 | 配置成员边界、内容标识、存储客户端和共享块缓存；显式边界的 CRS 转换使用 GDAL |
| `PostgresOutDbRasterSource` | 从 PostgreSQL 查询外部栅格路径，使用 GDAL 读取与渲染 | 配置连接池、查询函数及栅格文件访问环境 |

`stac` 包含 `agtiff`；固定镶嵌和 OutDB 通过 `raster` 启用。STAC 和固定镶嵌提供 `first`、`highest`、`lowest`、`mean`、`median` 像元组合方式，应用可通过 `TileQuery.pixel_selection` 选择请求使用的方式。

各入口的配置与处理流程见 [`stac.rs`](../src/sources/stac.rs)、[`mosaic_source.rs`](../src/sources/mosaic_source.rs)、[`outdb.rs`](../src/sources/postgres/outdb.rs)。数据库连接复用方式见[数据源与查询](sources.md)。

## 生成 WMTS 元数据

`build_capabilities(&service, &layers)` 生成 WMTS 1.0.0 GetCapabilities XML。`WmtsService` 描述服务标题、摘要和访问地址；每个 `WmtsLayer` 提供图层标识、瓦片 URL 模板、图片 MIME 类型、缩放范围和可选 WGS84 边界。应用将生成的 XML 与对应瓦片入口一起提供给客户端。

生成的矩阵集名为 `GoogleMapsCompatible`，使用 EPSG:3857、512 像素瓦片及 0 到 22 级矩阵。图层的 `min_zoom`、`max_zoom` 用于写出矩阵范围；配置时保持元数据与实际瓦片输出一致。实现及 XML 回归样例见 [`wmts.rs`](../src/render/wmts.rs) 和 [WMTS 测试](../tests/render/wmts.rs)。

## 验证与后续接入

原生渲染回归覆盖图像内容、尺寸与倍率、重复请求、并发任务、覆盖物和 PNG/JPEG/WebP 元数据。按[开发与构建](development.md#原生渲染构建)在目标图形环境运行串行原生测试，再使用业务样式验证字体、精灵图和瓦片资源的访问。

继续阅读[数据源与查询](sources.md)接入数据，或返回[文档导航](README.md)选择其他指南。
