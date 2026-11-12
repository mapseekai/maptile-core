# 瓦片测试数据

本目录保留可复用的合成测试样例，供源读取、元数据和栅格行为验证使用。

| 文件 | 用途 | 维护方式 |
| --- | --- | --- |
| `sample.pmtiles` | 在 `z=0/x=0/y=0` 保存 14 字节的 `test-tile-data`，验证 PMTiles 读取和缓存接入 | `tests/sources/pmtiles/local.rs::ensure_fixture()` 可重新生成；归档的声明格式为 MVT，载荷用于字节读取测试 |
| `sample.mbtiles` | 保留的 MBTiles 归档样例 | 当前 `tests/sources/mbtiles.rs::fixture()` 为每个测试生成独立的临时 PNG 归档 |
| `cog_uint8_rgba.tif` | 验证源 Alpha 输入检查 | 保留现有原件；完整原件生成配方待补充 |
| `cog_uint8_rgb.tif` | 验证 RGB COG 读取 | 可按下面的命令从 RGBA 原件生成 |

在本目录运行：

```sh
gdal_translate -of COG -b 1 -b 2 -b 3 -mask none cog_uint8_rgba.tif cog_uint8_rgb.tif
```

入门教程使用 `sample.pmtiles` 验证组件接入，见[读取第一张瓦片](../../docs/getting-started.md)。测试依赖版本以 [Cargo.toml](../../Cargo.toml) 和 [Cargo.lock](../../Cargo.lock) 为准。
