# 独立项目来源

提取日期：2026-11-11。

- 来源：MapSeek 的 `services/maptile/maptile-core`，固定提交 `2d01be1940c593e4b0a7369cfd9841ef6b923d22`。
- 从 97 个已跟踪 crate 文件建立独立 Git 仓库，不复制 MapSeek 完整历史、其他服务、凭据、用户级 Cargo 配置或构建产物。
- 保留原 API、版本 0.3.0、空的默认 features 和测试数据。将 workspace lint 转为本地 lint，补齐独立文档、CI、第三方声明及构建支持，并修正 Rustdoc 的失效链接；没有改变运行逻辑。
- MapSeek 的原路径和 server → core 依赖保持不变。本次没有删除其内嵌副本，也没有将服务改为新的 git/registry 依赖；未来接入需单独选择版本与验证。
- 经用户于 2026-11-11 明确选择，独立项目自有代码采用 MIT；第三方原版权与许可证保留。MapSeek 主仓库许可证不变。公开仓库为 `mapseekai/maptile-core`；本次没有发布 crates.io，manifest 保持 `publish = false`。

## 独立验证范围

默认 features 和 GeoParquet 为主要回归入口。`cloud,metrics,mlt`、GDAL/PROJ 和原生渲染需要分别验证，不能将某个 feature 的通过描述成全功能跨平台通过。PostGIS/S3 集成和原生图形测试需要明确准备独立测试环境。
