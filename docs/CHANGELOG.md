# maptile-core 版本变更

此页保留原发布记录的版本、日期和兼容性影响。历史内部结构可能已被后续实现替代，现行feature与调用方式见[核心库说明](../README.md)。未发布条目没有补造发布日期。

## 未发布

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
