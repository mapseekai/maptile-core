# 开发与构建

本仓库是独立的 Rust library crate，使用自己的 manifest、锁文件和 CI。开发时按目标应用启用的 features 准备环境并运行检查，让构建结果对应实际使用的能力。入口声明见 [Cargo.toml](../Cargo.toml)，自动化检查见 [CI 工作流](../.github/workflows/ci.yml)。

## 工具链与原生依赖

项目通过 [rust-toolchain.toml](../rust-toolchain.toml)固定 Rust 1.98.1，使用 Rust 2024 edition。准备 rustup 和本机编译工具后，在仓库根目录检查：

```sh
rustup show active-toolchain
rustc --version
cargo --version
```

按所选 feature 准备原生环境：

| 构建组合 | 环境要求 | 使用说明 |
| --- | --- | --- |
| 默认 | Rust、平台链接器与 C 编译工具 | 构建目录、默认读取栈和基础组件 |
| `geoparquet` / `duckdb` | C/C++ 工具链 | 编译 bundled DuckDB；几何查询准备相应扩展 |
| `local-vector` | `geoparquet` 的环境与 Spatial 扩展 | 使用扩展自带的矢量驱动 |
| `agtiff` / `stac` | PROJ 及匹配的运行时坐标数据 | 当前 `proj-sys` 构建要求系统 PROJ ≥9.6.2，或由 CMake 构建随包源码 |
| `raster` | GDAL 3.4 至 3.12、配套头文件及 PROJ | macOS CI 使用 GDAL 3.12，Ubuntu 24.04 使用发行版的 GDAL |
| `render`，macOS | C/C++、CMake、Ninja、Xcode Command Line Tools | 使用 Metal 后端 |
| `render`，Linux | C/C++、CMake、Ninja及 Vulkan 构建和运行环境 | 使用默认 Vulkan 后端 |
| `render-opengl` | Linux 的 C/C++、CMake、Ninja及 OpenGL 构建和运行环境 | 使用 OpenGL 变体 |

依赖解析版本记录在 [Cargo.lock](../Cargo.lock)。PROJ 的版本探测和源码回退逻辑由锁定版本的 `proj-sys` 构建脚本执行；运行时同时提供匹配的 PROJ 数据目录。

使用 macOS 上的 GDAL 3.12 时，请让 `GDAL_HOME` 和 `PKG_CONFIG_PATH` 指向同一安装。运行 `pkg-config --modversion gdal` 确认版本。GDAL 3.13 更改了字节类型名及栅格 I/O 参数结构，当前正式版 Rust `gdal 0.19` 无法用其头文件编译。升级 Rust 绑定前，`raster` 请使用上表的原生版本。

## 常规检查

在仓库根目录执行：

```sh
cargo fmt --all --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps
bash scripts/ci/maptile-render-cargo_test.sh
git diff --check
```

新增的入门示例可独立运行：

```sh
cargo run --locked --features pmtiles --example read_pmtiles
```

`cargo doc` 的结果位于 `target/doc/maptile_core/index.html`。加上 `--open` 可在本地浏览。开发约定与测试组织方式见 [AGENTS.md](../AGENTS.md)。

## 按功能验证

| 能力 | 检查命令 | 环境准备 |
| --- | --- | --- |
| 对象存储接口、指标与 MLT | `cargo test --locked --features cloud,metrics,mlt` | 按测试需要准备外部存储 |
| GeoParquet | `cargo test --locked --features geoparquet --lib` | 匹配的 Spatial/HTTPFS 扩展 |
| GeoParquet 与本地矢量 | `cargo test --locked --features local-vector --lib` | 匹配的扩展与 C/C++ 工具链 |
| GDAL 与 STAC 编译 | `cargo check --locked --all-targets --features raster,stac` | GDAL、PROJ 与开发库 |
| 渲染脚本行为 | `bash scripts/ci/maptile-render-cargo_test.sh` | Bash 与 Git |

GeoParquet 扩展的安装、校验和运行示例见[GeoParquet 与本地矢量](geoparquet.md)。CI 的 `geoparquet` 检查组启用 `local-vector`，覆盖 GeoParquet 与本地矢量路径。

记录验证结果时，同时记录实际 feature 组合和外部环境。编译检查、单元测试、数据库集成与原生图形运行分别验证不同层面的行为。

## PostgreSQL 集成验证

为测试准备独立的 PostGIS 数据库，将 `DATABASE_URL` 设置为该库的连接串，再运行对应集成目标：

```sh
cargo test --locked --features postgres --test postgres_pool --test postgres_table --test postgres_function
```

这些测试会创建测试表或函数，应使用专用测试库。测试辅助代码在缺少连接配置时会提前返回，因此数据库验证记录应包含实际连接环境。仓库 CI 使用独立 PostGIS 服务，配置见 [ci.yml](../.github/workflows/ci.yml)，测试入口见 [`tests/common/postgres.rs`](../tests/common/postgres.rs)。

连接竞争微基准可以单独运行：

```sh
cargo test --locked --features postgres --test postgres_table \
  registration_under_tile_load -- --ignored --nocapture
```

这个微基准使用 16 个连接、12 次并发打开同一小表，以及 8 路共 1600 次直接瓦片请求，观察冷启动和注册源时的连接竞争。结果的解释范围是该数据、构建模式与并发配置；容量评估应使用实际数据和部署负载。实现见 [`tests/sources/postgres/table.rs`](../tests/sources/postgres/table.rs)。

## 原生渲染构建

`render` 使用仓库内的 [MapLibre Native Rust 绑定](../third_party/maplibre-native-rs/README.md)。根 manifest 的相对 `path` 依赖将同一份修正传递给通过 Git 或 path 引用本库的应用，无需下游添加 `[patch]`。包版本仍为 `0.9.3`，它是本仓库保留的源码副本，不是新的 registry 发布版；默认 features 仍为空。

### 默认源码构建

在未设置 `MLN_PRECOMPILE`、`MLN_SYSTEM` 和 `DOCS_RS` 的环境中，普通 Cargo 命令会获取 Native 固定提交 `a33d3f00fb9dbf9117ce18edd799aa0ca5bcce5b` 及递归子模块，并编译启用 RTTI 的核心和 C++ 桥接。首次构建需要访问上游 Git 仓库。自动源码缓存位于绑定的 Cargo `OUT_DIR/native-source`，不会写入 `third_party` 或下游只读的依赖源码目录。

```sh
CARGO_BUILD_JOBS=2 cargo test --locked --features render \
  --test render_native -- --test-threads=1
```

独立应用可直接引用完整仓库检出：

```toml
[dependencies]
maptile-core = { path = "../maptile-core", default-features = false, features = ["render"] }
```

应用先用 `cargo build` 生成自己的锁文件，之后用 `cargo build --locked`。Git 依赖也会解析该检出内的相对路径；所选择的 Git revision 必须包含 `third_party` 绑定源码及 manifest 修正。

注册自定义文件源时，沿用本库的公开重导出：

```rust
use maptile_core::render::maplibre_file_source::{FileSource, ResourceRequest, Response};
```

同时直接依赖 registry 中的 `maplibre_native` 会引入另一个 Cargo 包身份，其类型和全局文件源注册表与本库使用的绑定不共享。应用只需声明 `maptile-core` 并使用上述路径。

需要独立管理源码缓存的 CI 可继续使用 [`maptile-render-cargo.sh`](../scripts/ci/maptile-render-cargo.sh)。它准备固定上游提交及递归子模块，设置 `MLN_LOCAL_REPOSITORY` 和源码构建模式，并清理预编译核心、系统核心和 `DOCS_RS` 编译跳过相关环境变量。

首次验证选择空的专用输出目录，分别运行 Debug 和 Release：

```sh
CARGO_TARGET_DIR=/tmp/maptile-core-native-debug \
  bash scripts/ci/maptile-render-cargo.sh test --locked --features render \
  --test render_native -- --test-threads=1
```

```sh
CARGO_TARGET_DIR=/tmp/maptile-core-native-release \
  bash scripts/ci/maptile-render-cargo.sh test --release --locked --features render \
  --test render_native -- --test-threads=1
```

该 CI 脚本的源码缓存默认位于 `.cache/maplibre-native-src/maplibre-native`。自定义 `MAPTILE_MLN_CHECKOUT` 时，使用以 `/maplibre-native` 结尾的专用构建缓存目录；脚本会重置其中的源码与子模块。直接设置 `MLN_LOCAL_REPOSITORY` 时也须提供此名称的完整匹配源码检出，并允许构建更新其子模块。

核心与桥接必须同时启用 RTTI，并保持 `NDEBUG` 一致；默认源码模式会按 Cargo 构建配置匹配两者。串行原生测试验证渲染器生命周期和真实图像输出，应与链接检查一起完成。渲染接口和尺寸配置见[样式与渲染](rendering.md)。

### 显式预编译输入

已有兼容的原生库时，根仓库和独立下游都可使用同一组环境变量：

```sh
MLN_PRECOMPILE=1 \
MLN_CORE_LIBRARY_PATH=/path/to/libmaplibre-native-core-amalgam.a \
MLN_CORE_LIBRARY_HEADERS_PATH=/path/to/maplibre-native-headers.tar.gz \
CARGO_BUILD_JOBS=2 cargo build --locked --features render
```

archive 和 headers 必须来自上述同一 Native revision、目标架构及图形后端，核心需 `MLN_WITH_RTTI=ON` 且定义 `NDEBUG`（推荐 Release，RelWithDebInfo 也满足）。预编译模式的桥接即使在 Rust Debug 下也定义 `NDEBUG`。构建脚本无法认证任意用户提供的二进制 ABI，输入配对由提供者负责。使用包含原生依赖的 amalgam archive；本机系统库仍须满足该目标的链接要求。

绑定不再自动下载旧的 0.9.3 原生资产；缺少任一预编译输入时会明确失败。没有兼容二进制时，移除 `MLN_PRECOMPILE` 以采用默认源码构建。仓库不包含原生 archive、headers 包或本机缓存。macOS 使用 Metal；Linux 使用所选后端及相应的原生环境。

分发包含原生渲染器的产物时，附带构建生成的 `mbgl-core.license` 和适用的第三方声明，详见[第三方声明](third-party-notices.md)。

## 提交文档和示例

公共接口、feature 或配置行为发生变化时，同步修改对应指南、Rustdoc 和示例。入口 README 保持项目定位、快速接入与导航；配置字段和执行约定放在其所属专题中。

提交前检查 Markdown 相对链接、Rustdoc 链接、代码示例和声明的 feature 条件，并按本次修改涉及的路径运行验证。历史版本与迁移事实保存在 [CHANGELOG.md](CHANGELOG.md)，项目提取记录保存在 [EXTRACTION.md](EXTRACTION.md)。

固定 PMTiles 归档的 HTTP 冷暖读结果、逐次 p95 和峰值 RSS 见[可复现的基准记录](catalog-stack-benchmark.md)。
