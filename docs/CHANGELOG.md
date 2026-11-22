# maptile-core 版本变更

此页保留原发布记录的版本、日期和兼容性影响。历史内部结构可能已被后续实现替代，现行feature与调用方式见[核心库说明](../README.md)。未发布条目没有补造发布日期。

## 未发布

### 版本化目录与瓦片读取

`CatalogService` 与 `TileStack` 提供原子目录发布、瓦片读取、应用中间层及导出。移除 `SourceManager`、源和样式的 `Catalog` 聚合及旧故障恢复接口。应用用 `SourceCatalog::reconcile` 提交发现清单，通过 `revoke` 撤销指定快照中的源实例。 [迁移指南](migration-0.3.md)

`CompositeDeclaration` 取代 `CompositeSource` 和 `LazyCompositeSource`。目录在发布时编译组合成员和描述符，并拒绝循环、重复叶子和超出深度限制的声明。`TileStack` 在本次快照中解析逗号分隔的临时组合。移除 `sources::mvt::merge` 和 `parse_composite_id_list`。 [组合策略](sources.md#组合多个-mvt-源)

自定义 `TileSource` 必须实现 `content_version()`，为每个打开实例提供稳定修订。缺少权威版本时，在打开时保存 `ContentVersion::new_generation()` 的返回值。移除 `clone_source` 和旧组合用于向下转型的 `Any` 超特征。重复发布同一实例时复用原 `Arc`，替换实例时独立打开源。 [源版本规则](sources.md#自定义数据源与内容版本)

### 样式重写与 HTTP 适配

`rewrite_style_for_native` 接收 `&CatalogSnapshot`，在同一目录代次读取普通源与组合的元数据。样式列表使用 `styles::snapshot`。 [样式与资源地址](rendering.md#样式与资源地址)

移除逐源 `get_tile_with_etag` 及 `TileEtagResponse`。HTTP 示例演示基于最终字节和表示元数据的 ETag 与条件请求。WMTS XML 生成代码和测试移到该示例，`/wmts.xml` 从目录读取图层元数据。渲染瓦片尺寸通过 `RenderRequest::TILE_SIZE` 取得，示例中的原生栅格使用归档尺寸。 [HTTP 示例](../examples/archive_http.rs) · [WMTS 实现](../examples/archive_http/wmts.rs)

### 数据库连接、空间边界与标识符

PostgreSQL TLS 显式选择 rustls ring provider，空默认 features 下无需宿主安装全局 provider。TCP `prefer` 现在真实尝试 TLS，服务器不支持时才回退；证书验证失败不会降级，`require` 保持证书链与主机名校验，`disable` 和 Unix socket 路径保留。锁定驱动仅支持 disable/prefer/require，私有 CA 须由系统信任或进程证书配置提供。

PostGIS 表的普通、批量和过滤读取保留 MVT buffer 内合法边界要素，普通和批量的 minzoom 行为不变。PostgreSQL 表/函数/OutDB 与独立 DuckDB 在单张、批量等对应入口拒绝非法 XYZ 和非有限 typed bbox，统一返回 `InvalidRequest`；有限 bbox 保留各源的既有范围语义，不自动把 west > east 改为日期线两侧。OutDB 与普通函数源复用标识符转义，合法带双引号的 schema/function 可准备和调用。

### 文件版本与固定镶嵌候选

固定 MosaicSource 根据成员各自的 WGS84 bbox 筛选每瓦片正面积相交候选，再执行上游 64 资产限制；显式合并 bounds 的 srid 不再覆盖成员范围。日期线两侧和递增的未折回经度可匹配，未知/非法/退化成员 bbox 保守保留并计入上限。范围纳入内容版本。全局统计仍使用整个成员集，受锁定上游 64 资产的统计发现限制，未改成单瓦片统计；需要统计的样式仍可能受远处坏文件影响。

FlatGeobuf 的 Unix 内容版本纳入完整 FileStamp，同大小、同 mtime 的不同 inode 替换文件重开后不再复用旧缓存；非 Unix 每次重开分配新代次，克隆共享版本。应用仍须重新打开并注册文件，热缓存不主动探测更新。MBTiles 相对路径固定到打开时的绝对别名路径，chdir 后源与克隆仍可读；文件、WAL 和 symlink 替换检测保留。

### watcher、图像编码与资源 URL

watcher 在默认 300ms 窗口后补发各事件种类的尾沿，避免迅速删除或原地更新缺少终态通知。通道和路径表均有上限；拥塞时重试根目录重扫通知，消费者需响应 `Modified(root)`。Drop 不等待消费者，容量不足时不保证交付所有剩余事件，零时长仍不合并。

公开 PNG/WebP 编码工具对 1、3、4 波段保留 mask 透明度，全有效的灰度/RGB PNG 色型保持不变，JPEG 仍无 alpha。Overlay 的缺失闭括号返回 None，无效颜色沿用默认值，非法/截断/溢出的 polyline 返回已解码完整前缀；合法编码行为和原生渲染池 panic guard 保留。

`rewrite_urls` 对 `?`/`&` 查询后缀按每个资源合并到 fragment 前，保留瓦片/字体占位符和已有绝对 URL。其他后缀继续原样追加，空串或单独 `?`/`&` 只补齐 base URL；参数值仍由调用方编码。此项扩展原先明确的原样后缀合约。

### DuckDB 生命周期与编码预算增强

DuckDB/GeoParquet 查询的中断句柄由借用连接的局部 guard 管理，同一互斥锁串行化 interrupt 与解除，连接先解除再析构或回池；未开始执行的阻塞闭包不发布句柄。该改动是针对源码析构窗口的预防性加固，未复现上游 UAF，未改动依赖。

公开 `to_png_async`、`to_jpeg_async`、`to_webp_async` 复用现有 `configure_blocking_concurrency` 额度，等待名额并在实际编码结束后释放；取消 waiter 不提前释放已运行工作的名额，编码错误仍为 Render。已占同一额度的阻塞任务应调用同步编码函数。本项为预算一致性增强，没有过载或 DoS 的复现结论。

### 缓存所有权与并发额度

`CatalogService` 按源策略缓存瓦片。有持久数据修订的 Postgres 表源可按 `cache_zoom` 缓存。函数源和缺少持久修订的表源每次查询数据库。移除两者的 `with_cache` 以及函数源的 `set_cache` 和 `cache` 公共接口。 [缓存规则](architecture.md#缓存身份与预算)

`PoolSettings` 新增 `batch_concurrency`，默认 2，批量 SQL 额度从进程全局改为每个池独立、池克隆共享。完整结构体字面量需要补充此字段或使用 `..PoolSettings::default()`。无法确认取消后的后端完成状态时只停止受影响池的批量准入；核实残留任务后再重建池。

新增 `transport::configure_blocking_concurrency(limit)`，应用可在首次相关工作前一次性设置共享阻塞任务额度；未配置时保留最多 16 的默认值。无效额度、重复配置或首次使用后配置返回 `Config`。阻塞任务和 SQL 清理仍持有各自额度直到工作结束。

### 样式读取预算与发现报告校验

本地样式文件上限为 16 MiB，构造和读取时均检查；实际读取也受限，防止文件检查后增长导致无界读取。超限返回带样式 ID、路径的 `StyleFile`，其 `source` 为 `PayloadTooLarge`。此前超过上限的样式需缩小文件或改用应用提供的 `StyleStore`。

发现清单中出现重复源 ID 或工厂返回不同的 ID 时，`SourceCatalog::reconcile` 返回 `Config`。构建和校验失败时保留当前目录。 [目录发布与生命周期](architecture.md#目录发布与生命周期)

### 错误分类

渲染队列入队超时改为 `CapacityExhausted`；本地样式存储对未知 ID 返回新增的 `StyleNotFound`，文件检查、读取和 JSON 解析失败返回 `StyleFile { id, path, source }`，底层 `source` 保留 `Io` / `Json`。调用方应更新原先匹配 `Render` / `Style`、直接匹配样式 `Io` / `Json` 或解析错误字符串的逻辑。

直接调用 `TileSource` 的默认栅格覆盖与像元查询方法会返回 `InvalidRequest`。`TileStack` 对能力不匹配的请求返回 `Unsupported`。

### 取消与输入边界

GeoParquet 远程打开的取消状态由 async 调用方持有，排队取消会跳过远程读取，在途取消连接到 DuckDB interrupt。明确安全的扩展安装和取消提示保留，其余后端诊断继续脱敏。

本地样式读取校验打开后的句柄，只接受普通文件。Unix 非阻塞打开防止 FIFO 替换挂起读取。`styles::snapshot` 返回的文件错误保留样式 ID、路径和底层原因。

### GeoParquet 远程版本

不可变 HTTP(S) 和 S3 对象通过 DuckDB HTTPFS 按需读取。有强 ETag 的 HTTP 对象可在打开时验证各个 Range 并保存为本地快照。移除旧回环 HTTP 服务、`open_http_with_client` 及 `open_object_store`。

按对象版本选择 `open_immutable_http`、`open_immutable_s3` 或 `open_http_at_etag`。未限定版本的 `open_http`、`open_http_with_headers` 和 `open_s3` 返回 `Unsupported`。部署侧负责约束 HTTPFS 的网络出口与重定向。 [固定远程对象版本](geoparquet.md#固定远程对象版本)

### WKB 输入校验

WKB→MVT 与 `wkb_bbox` 在调用 geozero 前，先以无分配的结构预检验证完整载荷、计数、子类型和维度。保留 16 MiB 输入上限，最多接受 64 层几何嵌套；截断、尾随字节、虚假大计数及会触发上游容量下溢的空环会返回错误或 `None`，不进入大容量预分配。有效 OGC/ISO WKB、EWKB、Z/M/ZM、SRID 和大小端组合有回归覆盖。

### 移除

本地字体/精灵图存储、发现、目录项及font-gen/sprite-gen生成器退出核心库。Resource服务承担分发，样式与渲染仍接受外部glyph/sprite URL。

### 编码变化

WKB→MVT与wkb_bbox改用geozero0.15（bbox为BoundsProcessor）。几何内容保留，面环方向符合MVT，同时接受EWKB、3D和GeometryCollection；旧版可能对此返回错误或None。二维bbox数值保持一致，但输出MVT不保证与旧版逐字节相同。

sources::mvt::proto重导出geozero的Tile/Feature/Layer/Value/GeomType，wire格式及Rust形状不变；encode/decode所需prost trait转到geozero使用的0.14。


### 新增验证

输入边界与3D/EWKB能力由 tests/wkb_mvt.rs 覆盖，Criterion 基准位于 benches/wkb_mvt_compare.rs。

## 0.3.0 — 2026-05-05

这是一次Rust规则审查后的破坏性升级。除CacheZoomRange的弃用别名外，该发布没有兼容垫片。

### 调用方必须调整

| API | 当时变化与迁移 |
| --- | --- |
| MaptileError | 尾部Box错误改Arc以支持Clone；Filter用From FilterError；枚举及FilterError为non_exhaustive，match要留兜底 |
| TileCache::new | 从直接Self改MaptileResult<Self>，处理容量乘法溢出，构造加`?`或明确unwrap |
| PostgresTableSource | Drop不再新建Tokio runtime；需要清理服务端函数时显式`close(self).await`，设置drop_function_on_close却未close会warn |
| CacheZoomRange::is_empty | 改名is_unbounded，旧名当时保留deprecated alias |

### 新增

溢出检查的tiles_per_axis/MAX_ZOOM支持到30，经prelude以MAX_TILE_ZOOM导出。scan_async及字体/精灵图、栅格decode/encode异步包装将CPU任务放spawn_blocking；字体/精灵图包装后来随资源功能移除，不能据历史记录调用。SourceManager替换同ID时新增tracing告警。

### 防护

当时自写WKB parser增加safe_capacity/read_array和MAX_WKB_DEPTH=4；该实现后来由geozero替代。CQL2 JSON进入serde前检查MAX_FILTER_DEPTH=32；SQL生成校验列白名单和类型。ImageGuard负责MLNImageData释放，tile math拒绝z>30；watch使用有界队列/try_send，毒化锁恢复，丢事件告警。

GeoParquet归档内存载入上限4M行；RGBA尺寸计算使用checked_mul避免宽×高×4溢出。

### 性能与并发

属性去重用typed TagValueKey替代格式化字符串，减少逐属性分配；线/环坐标即时投影而非中间Vec。连续raster数组走chunks_exact_mut/zip，非标准布局保留索引路径。WMTS写入用writeln，XML转义单次扫描；overlay解析用迭代器代替小Vec。

OutDB在spawn_blocking前释放全局dataset-cache锁，但保持独立单dataset排他锁。当时渲染池增加RendererHandle RAII，free列表用std::sync::Mutex供Drop归还，预热前释放bucket锁；这不是现行池结构保证。

## 0.2.0 — 2026-05-04

此破坏性版本对齐martin-core0.5.3的缓存与观测接口，没有deprecated垫片。

### 兼容性

构造签名从`TileCache::new(max_size_mb, ttl_seconds)`变为`TileCache::new(max_size_mb, expiry: Option<Duration>, idle_timeout: Option<Duration>)`。例如旧`new(512,3600)`改为`new(512,Some(Duration::from_secs(3600)),None)`；0.3.0还需处理Result。

默认ETag由SHA-1 hex改xxh3_128加base64-url，瓦片字节不变，HTTP缓存首次升级访问会重验；自行覆盖get_tile_with_etag的源不受影响。当时moka Arc错误包装导致cache-miss错误被转换为Backend(io::Error::other(display))，保留显示却丢结构；0.3.0的Clone错误改造随后解决这一阶段问题。

### 缓存与指标

新增get_or_insert合并在途miss，invalidate_source按源选择性失效（最终一致），run_pending_tasks供测试等待维护。metrics feature暴露maptile_cache_requests_total{cache,result}及maptile_tile_cache_requests_total{cache,result,zoom}；SourceManager::get_tile和TileCache::get_or_insert加入源/坐标tracing。

默认ETag不再用sha1，但当时PMTiles/composite的非ETag散列仍保留依赖。ZOOM_LABELS预生成31个标签；缓存开启support_invalidation_closures支撑按源失效。

## 0.1.0

首次发布发生在建立此变更记录之前，原文未提供日期；不推断发布日期或额外功能清单。
