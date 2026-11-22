# 地图与栅格渲染

`maptile-core` 提供两条图像生成路径：MapLibre Native 按地图样式合成矢量、栅格和标注图层；COG、STAC 与 OutDB 数据源按 Raster Style 处理影像并生成瓦片。应用可以按数据和输出目标组合这些组件。

公共入口见 [`render`](../src/render/mod.rs)、[`styles`](../src/styles/mod.rs) 和 [`raster::style`](../src/raster/style/mod.rs)。本页面向负责接入样式、图片渲染和影像瓦片的 Rust 开发者。

## 选择渲染入口

| 目标 | 主要接口 | Feature |
| --- | --- | --- |
| 读取样式、调整资源 URL | `LocalStyleStore`、`StyleStore`、`rewrite_urls`、`rewrite_style_for_native` | 默认构建 |
| 构造地图请求、解析视口 | `RenderRequest`、`RenderCamera`、`StaticTarget` | 默认构建 |
| 按地图样式输出图片 | `RendererPool`、`MapLibreRenderer` | `render` |
| 单幅 COG 动态切片 | `AsyncCogSource` | `agtiff` |
| 按目录搜索并镶嵌 COG | `StacSource` | `stac` |
| 固定成员镶嵌 | `MosaicSource` | `agtiff` |
| 读取 PostGIS OutDB 影像 | `PostgresOutDbRasterSource` | `postgres,raster` |

默认构建提供样式管理和请求构造。执行地图图片渲染时启用 `render`，由工作线程持有原生渲染器。WMTS XML 由应用适配器生成。 [功能开关](features.md) · [原生渲染构建](development.md#原生渲染构建) · [WMTS 元数据示例](#生成-wmts-元数据)

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

用 `styles::snapshot(&store, base_url, query_string).await` 生成 `StyleInfo` 列表。`base_url` 为 `Some` 时填入 `<base>/styles/<id>/style.json` 与查询后缀。如果服务需要在同一个响应中列出样式和源，应用可从 `CatalogSnapshot::descriptors()` 获取源列表并自行组装响应。 [样式列表实现](../src/styles/catalog.rs)

调用方可按 `MaptileError` 变体区分失败原因：

| 情况 | 错误 |
| --- | --- |
| `get` 请求未注册的样式 ID | `StyleNotFound(id)` |
| 构造时检查文件失败，或 `get` 读取文件失败 | `StyleFile { id, path, source }`，`source` 为 `Io(error)`，通过 `error.kind()` 区分文件不存在、权限不足等 |
| `get` 解析 JSON 失败 | `StyleFile { id, path, source }`，`source` 为 `Json(error)`，可读取 `classify()`、`line()`、`column()` |
| 构造或读取时样式文件超过 16 MiB | `StyleFile { id, path, source }`，`source` 为 `PayloadTooLarge { limit: 16777216 }` |

`StyleFile.source.as_ref()` 可用于匹配上述底层错误。`styles::snapshot` 同样返回带样式 ID 与路径的错误。已注册样式的文件随后被删除时，底层错误为 `Io(NotFound)`。应用对外输出错误时应隐藏内部文件路径。

样式路径必须指向普通文件，指向普通文件的符号链接仍可使用。构造和读取都会校验类型；读取校验的是已打开句柄，Unix 上使用非阻塞打开，避免注册后路径被替换为 FIFO 时阻塞。目录和 FIFO 返回带上下文的文件错误。

本地样式文件最大为 **16 MiB（16,777,216 字节，包含边界）**。构造和读取都会检查文件长度。实际读取最多取上限加 1 字节，超限即拒绝解析，防止检查后文件增长绕过限制。样式列表使用同一读取路径。对超过上限的文件，应用可缩小文件或提供自己的 `StyleStore` 实现。

`rewrite_urls(&style, base_url, query_string)` 将以下字段中以 `/` 开头的资源地址转换为绝对 URL：

- `sources[*].url` 与 `sources[*].tiles[*]`。
- `glyphs` 和字符串形式的 `sprite`。

`query_string` 以 `?` 或 `&` 开头时，参数会合并到每个资源各自的查询串，并放在 `#fragment` 之前。例如 `/data/a.json?v=1#meta` 与 `?key=abc` 合并为 `/data/a.json?v=1&key=abc#meta`；没有查询串的 URL 则使用 `?`。已有完整 URL 保持其配置值，瓦片、字体占位符保持原字节。

传入空字符串、单独的 `?` 或 `&` 可只补齐域名。参数值应已完成 URL 编码，例如值中的 `#` 应写为 `%23`；函数不会解析或编码参数。其他后缀保留直接追加的既有用途，例如 `.json` 会追加到完整资源字符串末尾，不参与查询串或片段合并。

应用从 `catalog.catalog().snapshot()` 取得目录快照，再调用 `rewrite_style_for_native(&style.value, base_url, &snapshot)`。函数对整个样式使用同一代快照，按普通源与组合的实际格式将已注册的 `/data/<id>` 或 `/data/<id>.json` 引用转成瓦片 URL 模板。函数还用描述符补充缺省的缩放范围、边界和署名。应用负责配置瓦片、字体和精灵图的可访问地址，并用描述符中的图层名填写 `source-layer`。 [样式重写实现](../src/styles/store.rs) · [数据源与查询](sources.md)

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

`render` feature 提供路径、标记和 GeoJSON 覆盖物，可配置颜色、线宽、填充与标记尺寸。渲染器先生成地图图像，再绘制覆盖物并编码。公开类型和解析函数见 [`overlay.rs`](../src/render/overlay.rs)。缺失闭括号的路径和标记返回 `None`；无效颜色沿用默认颜色。`decode_polyline` 遇到非法字符、截断或整数溢出时返回此前完整解码的点，保留既有的前缀解码行为。

## 输出尺寸与并发

`RenderFormat` 提供 PNG、JPEG 和 WebP。返回的 `RenderedImage` 包含编码字节、格式和实际像素尺寸；`content_type()` 返回对应 MIME 类型。

| 配置 | 当前值或要求 | 使用方式 |
| --- | --- | --- |
| `for_tile` 逻辑尺寸 | `RenderRequest::TILE_SIZE`（512）作为宽和高 | 适合按 XYZ 渲染地图瓦片 |
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

入队等待超时返回 `MaptileError::CapacityExhausted`，应用可据此实施退避或返回繁忙响应。队列已关闭或工作线程丢失响应仍返回 `MaptileError::Render`。

应用需要为某次请求创建独立原生上下文时，可以设置 `request.isolate_native_state = true`。这一选项由可信应用代码决定，适合为请求配置独立的资源访问上下文，字段语义见 [`RenderRequest`](../src/render/types.rs)。

启用 `raster` 后，独立的 `raster::encode::to_png_async`、`to_jpeg_async`、`to_webp_async` 与传输压缩、解压共用 `configure_blocking_concurrency` 设置的阻塞任务预算。它们会等待可用名额；取消尚未开始的等待不会占用名额，取消已开始任务的等待者也不会提前释放名额，实际编码结束后才释放。不要在已占用同一预算的阻塞任务中同步等待这些 async 包装；该场景应直接调用同步编码函数。

独立栅格编码工具对 1、3、4 波段 PNG/WebP 均将无效 mask 像素编码为透明像素。全有效的单波段 PNG 保留灰度格式，全有效的三波段 PNG 保留 RGB；需要透明度时增加 alpha，四波段保留其 alpha 并叠加 mask。JPEG 仍输出 RGB，忽略 mask 和 alpha。所有同步和异步编码入口都要求 `mask.shape() == [height, width]`，包括全有效 mask 与 JPEG；尺寸不匹配返回 `MaptileError::Render`，不按元素数量或内存布局截断。直接使用返回 `Vec<u8>` 的 `rgba_from_raster` 时，调用方须保证这一前置条件，违反时在 Debug 和 Release 均会 panic。

## COG 栅格切片

启用 `agtiff` 后，`AsyncCogSource::open` 接收 `AsyncCogSourceConfig`。其中 `CogObjectSource` 提供本地绝对路径、HTTP(S) URL、已配置的 `Arc<dyn ObjectStore>` 与对象键三种入口。

每次切片请求通过 `TileQuery.style` 提供 `EffectiveStyle`。应用先用 `RasterStyle::from_json` 或 `from_value` 解析 Raster Style 文档，再通过 `EffectiveStyle::from_raster_style` 取得可用于查询和缓存的样式。选择波段、渲染器、拉伸、色带、重采样和输出参数时，以所用数据和样式声明为准。

`EffectiveStyle` 保存样式文档、瓦片尺寸、输出格式和样式散列；COG 适配器将其传给 async-geotiff，并在 `TileResponse.info` 中记录实际 PNG、WebP 或 JPEG 格式。实现见 [`style/resolve.rs`](../src/raster/style/resolve.rs)、[`agtiff_style.rs`](../src/raster/agtiff_style.rs) 与 [`cog.rs`](../src/sources/raster/cog.rs)。

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
| `MosaicSource` | 应用提供固定的 `MosaicMember` 列表，使用 async-geotiff 镶嵌 | 配置成员边界、内容标识、存储客户端和共享块缓存；显式边界的 CRS 转换使用 PROJ |
| `PostgresOutDbRasterSource` | 从 PostgreSQL 查询外部栅格路径，使用 GDAL 读取与渲染 | 配置连接池、查询函数及栅格文件访问环境 |

`stac` 包含 `agtiff`；固定镶嵌通过 `agtiff` 启用。OutDB 同时需要 `postgres` 和 `raster`。STAC 和固定镶嵌提供 `first`、`highest`、`lowest`、`mean`、`median` 像元组合方式，应用可通过 `TileQuery.pixel_selection` 选择请求使用的方式。

OutDB 每次请求都会查询当前资产，因此始终绕过瓦片结果缓存。省略 `cache_zoom`，或通过 `CacheZoomRange::disabled()` 显式禁用。对应 JSON 值为 `{"minzoom":255,"maxzoom":0}`。启用缓存的旧配置值仍可反序列化，打开源时会收到配置错误。

固定镶嵌先按成员 WGS84 范围筛选每瓦片候选，但全局统计仍使用全部成员，并继续受上游默认 64 个资产的统计发现限制。跨日期线、未知范围和缓存版本规则见[成员范围说明](sources.md#固定栅格镶嵌与成员范围)。

各入口的配置与处理流程见 [`stac.rs`](../src/sources/raster/stac.rs)、[`mosaic.rs`](../src/sources/raster/mosaic.rs)、[`outdb.rs`](../src/sources/postgres/outdb.rs)。数据库连接复用方式见[数据源与查询](sources.md)。

## 生成 WMTS 元数据

`archive_http` 示例的 `/wmts.xml` 返回 WMTS 1.0.0 GetCapabilities XML。应用适配器用已配置的公开地址生成服务与瓦片链接，并从目录描述符读取图层标识、MIME 类型、缩放范围和 WGS84 边界。示例中的 XML 组装和回归测试位于同一模块。 [HTTP 路由](../examples/archive_http.rs) · [WMTS 实现](../examples/archive_http/wmts.rs)

示例生成的 `GoogleMapsCompatible` 矩阵集使用 EPSG:3857，覆盖 0 到 22 级，并按归档的最大缩放级别扩展至最高 30 级。图层仍声明归档的缩放范围。MVT 与 MLT 使用 `RenderRequest::TILE_SIZE`（512）作为逻辑瓦片尺寸。PNG、JPEG 和 WebP 使用打开归档时读取或探测得到的 `tile_size`，例如原生 256 像素的 PNG。已发布的栅格源缺少有效尺寸时，示例路由返回 422。 [XML 回归样例](../examples/archive_http/wmts_golden.xml)

## 验证与后续接入

原生渲染回归覆盖图像内容、尺寸与倍率、重复请求、并发任务、覆盖物和 PNG/JPEG/WebP 元数据。按[开发与构建](development.md#原生渲染构建)在目标图形环境运行串行原生测试，再使用业务样式验证字体、精灵图和瓦片资源的访问。

继续阅读[数据源与查询](sources.md)接入数据，或返回[文档导航](README.md)选择其他指南。
