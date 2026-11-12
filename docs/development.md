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
| 默认 | Rust、平台链接器与 C 编译工具 | 构建默认数据源和基础组件 |
| `geoparquet` / `duckdb` | C/C++ 工具链 | 编译 bundled DuckDB；几何查询准备相应扩展 |
| `local-vector` | `geoparquet` 的环境与 Spatial 扩展 | 使用扩展自带的矢量驱动 |
| `agtiff` / `stac` | PROJ 及匹配的运行时坐标数据 | 当前 `proj-sys` 构建要求系统 PROJ ≥9.6.2，或由 CMake 构建随包源码 |
| `raster` | GDAL、对应开发库及 PROJ | 提供 GDAL 相关栅格路径 |
| `render`，macOS | C/C++、CMake、Ninja、Xcode Command Line Tools | 使用 Metal 后端 |
| `render`，Linux | C/C++、CMake、Ninja及 Vulkan 构建和运行环境 | 使用默认 Vulkan 后端 |
| `render-opengl` | Linux 的 C/C++、CMake、Ninja及 OpenGL 构建和运行环境 | 使用 OpenGL 变体 |

依赖解析版本记录在 [Cargo.lock](../Cargo.lock)。PROJ 的版本探测和源码回退逻辑由锁定版本的 `proj-sys` 构建脚本执行；运行时同时提供匹配的 PROJ 数据目录。

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
cargo run --locked --example read_pmtiles
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
cargo test --locked --test postgres_pool --test postgres_table --test postgres_function
```

这些测试会创建测试表或函数，应使用专用测试库。测试辅助代码在缺少连接配置时会提前返回，因此数据库验证记录应包含实际连接环境。仓库 CI 使用独立 PostGIS 服务，配置见 [ci.yml](../.github/workflows/ci.yml)，测试入口见 [`tests/common/postgres.rs`](../tests/common/postgres.rs)。

连接竞争微基准可以单独运行：

```sh
cargo test --locked --test postgres_table \
  registration_under_tile_load -- --ignored --nocapture
```

这个微基准使用 16 个连接、12 次并发打开同一小表，以及 8 路共 1600 次直接瓦片请求，观察冷启动和注册源时的连接竞争。结果的解释范围是该数据、构建模式与并发配置；容量评估应使用实际数据和部署负载。实现见 [`tests/sources/postgres/table.rs`](../tests/sources/postgres/table.rs)。

## 原生渲染构建

[`maptile-render-cargo.sh`](../scripts/ci/maptile-render-cargo.sh)准备固定上游提交及递归子模块，并把 Cargo 构建指向这一份 MapLibre Native 源码。它设置源码构建模式，清理预编译核心、系统核心和 `DOCS_RS` 编译跳过相关环境变量。

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

默认源码缓存位于 `.cache/maplibre-native-src/maplibre-native`。自定义 `MAPTILE_MLN_CHECKOUT` 时，使用以 `/maplibre-native` 结尾的专用构建缓存目录；脚本会重置其中的源码与子模块。固定提交和源码地址的默认值以脚本为准。

Release 构建需要核心与 C++ 桥接的 `NDEBUG` 一致。串行原生测试验证渲染器生命周期和真实图像输出，应与链接检查一起完成。渲染接口和尺寸配置见[样式与渲染](rendering.md)。

分发包含原生渲染器的产物时，附带构建生成的 `mbgl-core.license` 和适用的第三方声明，详见[第三方声明](third-party-notices.md)。

## 提交文档和示例

公共接口、feature 或配置行为发生变化时，同步修改对应指南、Rustdoc 和示例。入口 README 保持项目定位、快速接入与导航；配置字段和执行约定放在其所属专题中。

提交前检查 Markdown 相对链接、Rustdoc 链接、代码示例和声明的 feature 条件，并按本次修改涉及的路径运行验证。历史版本与迁移事实保存在 [CHANGELOG.md](CHANGELOG.md)，项目提取记录保存在 [EXTRACTION.md](EXTRACTION.md)。
