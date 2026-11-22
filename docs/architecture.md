# 架构与生命周期

`maptile-core` 将数据访问、瓦片缓存、样式处理和地图渲染组织为可组合的 Rust 模块。应用构造具体数据源与配置，管理这些对象的生命周期，并将库返回的字节、元数据或图像接入自己的交互与服务流程。

公共模块见 [`src/lib.rs`](../src/lib.rs)，常用类型的导出入口见 [`prelude`](../src/prelude.rs)。

## 组件职责

| 维护任务 | 源码入口 |
| --- | --- |
| 实现驱动、声明查询能力或维护已有 `TileSource` | [TileDriver 与描述符](../src/control/driver.rs)、[TileSource](../src/sources/driver.rs)、[请求与批量校验](../src/sources/request.rs) |
| 发布目录快照、固定组合成员、排空旧源 | [目录发布](../src/control/catalog.rs)、[组合编译与读取](../src/control/composition/mod.rs)、[生命周期](../src/control/lifecycle.rs) |
| 追踪一次瓦片读取、应用层或跨页导出 | [TileStack 入口](../src/service/mod.rs)、[CatalogService 读引擎](../src/service/engine.rs)、[导出会话](../src/service/export.rs) |
| 调整缓存或调查同一瓦片的并发读取 | [TileCache](../src/cache/tile_cache.rs)、[按目录代次合并在途读取](../src/cache/flights.rs) |
| 修改共用的坐标、瓦片结果或底层 I/O 检查 | [核心类型与坐标换算](../src/core/mod.rs)、[文件身份与 HTTP Range](../src/io/mod.rs)、[传输编解码](../src/transport.rs) |
| 修改具体后端 | [DuckDB 几何源](../src/sources/duckdb/mod.rs)、[栅格数据源](../src/sources/raster/mod.rs)、[Postgres](../src/sources/postgres/mod.rs)、[其他归档源](../src/sources/mod.rs) |
| 扫描文件、通知变更，将发现结果映射为目录更新 | [discovery](../src/discovery.rs)、[watch](../src/watch.rs) |
| 调整过滤、样式、影像处理或地图渲染 | [filter](../src/filter.rs)、[styles](../src/styles/mod.rs)、[raster](../src/raster/mod.rs)、[render](../src/render/mod.rs) |
| 接入 HTTP 表示协商、条件请求和元数据 | [HTTP 示例](../examples/archive_http.rs)、[WMTS XML](../examples/archive_http/wmts.rs) |

应用负责鉴权、可见范围、配置、HTTP 路由、表示协商及对外地址。应用可以通过 `CatalogSnapshot::descriptors()` 枚举同一代的普通源与组合，并为源的 TileJSON 补充绝对瓦片地址。

## 瓦片读取过程

一次读取固定当前 `CatalogSnapshot`、源描述符及组合成员。`TileStack` 检查 XYZ、查询能力和请求预算，再按注册顺序进入 `TileLayer`，并按逆序返回。没有自定义层时直接进入默认 `CatalogService`。短路或改写的瓦片仍须通过 16 MiB 大小检查。透传规范结果时，同一字节视图和编码可以复用已完成的校验。

`TileQuery::style` 和 `style_hash` 适用于声明 `raster_style` 能力的驱动，例如 COG、固定镶嵌、STAC 和 PostGIS OutDB。目录读取在进入应用层或读取缓存前为能力不匹配的请求返回 `Unsupported`。自定义 `TileDriver` 在 `SourceDescriptor.capabilities` 中声明能力，自定义 `TileSource` 实现 `supports_raster_style()`。`CompositeDeclaration` 在发布时固定本代成员，只有已解析成员均支持样式时，组合才能接受该查询。

`TileOutcome::NoTile`、`Present` 与 `Partial` 分别表示无瓦片、完整瓦片和组合降级。合法零字节 MVT 可作为 `Present`。部分结果可包含瓦片，也可没有瓦片。服务端必须对部分结果发送 `Cache-Control: no-store`，不生成强 ETag，并在条件请求时返回部分结果而非 304。

`read_batch` 保留输入顺序和空位，`BatchPage.consumed` 表示已处理的坐标数。调用方从该位置继续。`begin_export` 固定跨页的目录条目；仅声明支持一致导出的源可使用。`read_export_page` 的游标检查源修订和请求身份，调用方必须保留稳定的坐标序列、相同的层配置，并对每次请求重新鉴权。临时组合从去掉每个成员首尾空白后的 ID 按输入顺序生成身份。例如 `a,b` 和 ` a, b ` 共用描述符 ID、缓存、在途读取和游标。

一致导出（`begin_export`、`read_export_page`）逐瓦片读取时可以命中在线缓存。命中会更新该瓦片的访问频率和空闲过期时间（TTI）。只有在线请求也在等待同一份完整瓦片时，这次回源结果才会写入在线缓存。因此连续两次导出同一个冷坐标可能各自回源。单瓦片读取和 `read_batch` 的逐瓦片回退路径会照常缓存完整结果。

## 缓存身份与预算

`CatalogService` 的规范缓存键包含 schema 版本、源和组合修订、调用方指定的可见范围、XYZ、有效样式及镶嵌选择。过滤、bbox 和自定义参数按实现走直接读取。缓存只保存完整 `Present`，不保存部分结果和错误。自定义层改写返回内容时，改写值不会回填规范缓存。层应同步改写实际格式和编码，HTTP 层针对最终字节计算 ETag。

缓存用驱动声明的源修订隔离不同的数据版本。应用更新数据或影响响应的配置时，打开并发布具有新修订的源。远端归档需要不可变对象、固定版本或强 ETag，确保同一次打开的多个 Range 属于同一版本。 [数据源版本与接入](sources.md#自定义数据源与内容版本)

`TileCache::new` 接收按 MiB 计量的容量、TTL 和 TTI。容量不包含连接池、渲染器和其他分配。`TileStack` 首次读取通过缓存公开接口写入的压缩瓦片时会校验负载。校验结果只适用于该缓存条目。缓存命中不主动检查文件变化，更新数据后先打开新源并发布新目录。`invalidate_source` 和 `invalidate_all` 发起异步维护，测试可通过 `run_pending_tasks().await` 等待。

## 并发额度

COG、Mosaic 和 STAC 的瓦片渲染，以及 COG 的点查和覆盖导出，共享进程内的 64 个栅格请求名额。直接调用数据源和通过 `TileStack` 读取遵循相同规则，名额用尽时返回 `CapacityExhausted`。调用方取消读取后，仍在执行的原生工作继续持有名额和对应的源租约。

取消 COG 点查或覆盖导出会停止等待对象数据。若取消时可能还有原生解码或投影在执行，async-geotiff 0.13 仅提供当前 reader 的 CPU 使用计数。该 reader 暂时拒绝新请求，直到已提交的工作排空。其他已打开的 COG 可以继续读取。

`transport::configure_blocking_concurrency(limit)` 配置本 crate 经统一阻塞任务入口运行的解码、编码和其他 CPU/阻塞工作的进程共享额度。应用应在打开源或启动相关异步工作前调用一次；未配置时默认使用可用并行度，最多 16。显式配置可超过 16，须为正数且不超过 Tokio 信号量上限。重复配置或首次使用后配置返回 `Config`。同步编解码入口不经过此额度，原生渲染器仍使用其独立的池配置。

`raster::encode::to_png_async`、`to_jpeg_async`、`to_webp_async` 也通过上述共享额度准入。取消尚未开始的等待不会占用额度，已经开始的编码会持有额度至实际结束；已占同一额度的阻塞闭包应调用同步编码函数，避免同步等待异步包装。该统一准入是预算一致性增强，不代表已复现过载或 DoS。

Postgres 批量查询由每个连接池的 `PoolSettings.batch_concurrency` 控制，默认 2；池的克隆共享额度，分别调用 `connect` 创建的池互相独立。应用需将各池额度合计计入数据库预算。该额度在获取批量连接前占用；普通在线查询不占批量额度，但仍与批量查询共享池的连接数上限。

取消等待者不会提前归还仍在运行的阻塞任务或 SQL 清理任务的额度。数据库只有确认后端完成才归还额度；无法确认时停止该池的新批量准入，其他池不受影响。此时应先核实数据库残留任务，再重建受影响的池，避免旧任务和新额度叠加。

## 目录发布与生命周期

`catalog.update()` 基于当前快照创建更新。通过 `put`、`remove` 和 `composite` 编辑，再调用 `publish`。发布前检查组合依赖，成功后原子切换到新代次。并发发布遇到 `VersionChanged` 时，调用方从新快照重建更新。已固定的请求和导出会话继续读取旧代次。`revoke` 接收观察到的快照，可单独取消并排空该代源。`shutdown` 同时停止发布、读请求和仍有租约的旧代源，超过期限后可再次调用以等待排空。

扫描前调用 `catalog.snapshot().generation()` 记录预期代次。用 `discovery::scan` 或 `scan_async` 获取清单，把该代次与清单传给 `SourceCatalog::reconcile`。扫描或构建期间发生并发发布时，重新从当前代次扫描。应用为每个发现项提供稳定的 ID、数据集身份与修订：

- `Inventory::Complete` 更新所列项，并下线该命名空间中消失的源，保留 ID 与数据集的绑定。
- `Inventory::Partial` 更新所列项，但不删除未列出的绑定。
- `Inventory::Failed` 和 `Unchanged` 保留当前目录。

为本地 GeoParquet、FlatGeobuf、Parquet 归档或自定义同步驱动提供发现工厂时，使用 `control::open_blocking`。即使调用方取消发现，目录关闭也会等待已提交的阻塞任务结束。工厂自行启动的其他后台任务需要由工厂或驱动持有并排空。

ID 冲突或工厂返回不同的 ID 会拒绝更新。同一数据集恢复时可以重新使用原 ID；要把该 ID 分给其他数据集，须先显式发布 `CatalogUpdate::remove`。修订未变时复用当前源。扫描本身无法保证是完整清单，只有确认扫描完整后才传入 `Complete`。watcher 的通知可能合并，应用接收通知后应重新扫描，不能把它当作无损事件日志。

发布新版本前准备好不可变文件或对象，打开并校验后一次发布。等待旧代请求和导出结束后再删除旧资源。GeoParquet 和 FlatGeobuf 使用共享查询池，`start_close` 停止准入，`close().await` 等待阻塞查询及文件租约退出，详见[矢量文件指南](geoparquet.md)。

## 数据库与图像输入边界

PostgreSQL 连接显式选择 rustls ring provider，无需宿主初始化全局 provider。`prefer` 在 TCP 上先尝试 TLS，只有服务器明确不支持 TLS 才回退；`require` 必须通过证书链与主机名校验。`disable` 保持明文，Unix socket 可用 disable/prefer；锁定驱动仅接受这三种模式，详见 [TLS 连接](sources.md#tls-连接)。

PostGIS 表的普通、批量和过滤路径统一由 SQL 保留 MVT buffer 内的相交要素；普通和批量的 minzoom 短路不变。PostgreSQL 表/函数/OutDB 的坐标和 typed bbox 入口拒绝非法 XYZ 或非有限 bbox，返回 InvalidRequest；有限 bbox 保留各源既有语义，不自动新增日期线拆分。函数与 OutDB SQL 使用统一标识符转义，支持名称中的合法双引号。

PNG/WebP 编码将 1、3、4 波段的无效 mask 像素编码为透明像素，JPEG 继续忽略 alpha。Overlay 的括号、UTF-8 颜色和 polyline 整数边界通过解析检查处理，原生池的 panic guard 保留；URL 查询后缀按资源自身查询串合并到 fragment 前，并保留非查询后缀的直接追加用途。兼容性细节见[样式与渲染](rendering.md)。

## 错误与观测

[`MaptileError`](../src/error.rs)区分源状态、瓦片查找、配置、过滤和后端错误。应用可依据错误变体决定响应、重试或记录方式，并在对外输出时处理内部连接和文件上下文。

`TileStack` 根据源描述符校验栅格覆盖导出与像元查询能力，能力不匹配时返回 `Unsupported`。直接调用 `TileSource` 的默认覆盖和像元查询方法会返回 `InvalidRequest`。

渲染队列入队等待超时返回 `CapacityExhausted`。本地样式存储对未知样式 ID 返回 `StyleNotFound`，文件访问、大小检查与 JSON 解析失败返回带 ID、路径和底层原因的 `StyleFile`。`styles::snapshot` 读取样式时传播这一错误。 [样式与渲染](rendering.md#样式与资源地址)

应用可在 `TileLayer` 中记录请求级 `tracing`。启用 `metrics` 后，`TileStack` 的在线请求每次查询规范缓存计一次命中或未命中。导出、绕过缓存的请求以及合并并发读取时的内部复查均排除在此计数外。计数器在首次访问时注册到默认 Prometheus registry，应用负责抓取和暴露。连接数、源数量和原生渲染队列容量由应用按部署预算统一配置。 [指标定义](../src/metrics.rs)
