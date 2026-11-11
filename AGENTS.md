# maptile-core 开发约定

这是独立 Rust library crate。功能、构建及原生依赖见 README.md，GeoParquet 生命周期和安全边界见 docs/geoparquet.md。

- 保持空的默认 features，不将 MapSeek 业务身份、配置解析或 HTTP 路由引入核心库。
- Rust 源文件和模块使用 snake_case。已有源码对应同名测试文件；同一源码的用例放在同一文件，不按场景增设重复测试文件。
- 改动公共 API 时同步示例、测试和文档。不要用全局配置掩盖独立构建问题。
- 修改后运行 cargo fmt --all --check、相关 cargo test --locked、cargo clippy --locked --all-targets 及 git diff --check。
- GeoParquet 测试需匹配的 Spatial/HTTPFS 扩展；缺失外部服务或默认忽略的测试不能当成通过。macOS 不支持 render-opengl，不使用 --all-features。
- 保留 LICENSE、third_party 和源文件的归属声明；不得自行更换项目许可。公开发布和 crates.io 发布需明确授权。
