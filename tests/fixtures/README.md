# 瓦片测试数据

保留本目录的合成测试样例，不要替换为用户业务数据。

- `sample.pmtiles`：`tests/sources/pmtiles/local.rs::ensure_fixture()` 可重新生成，包含 z=0/x=0/y=0 的单张无压缩 MVT。
- `sample.mbtiles`：保留已有归档。当前 `tests/sources/mbtiles.rs::fixture()` 为每个测试生成独立的临时 PNG 归档，不依赖这个共享文件。
- `cog_uint8_rgba.tif`：保留原件，用于拒绝源 Alpha 的测试；本仓库没有完整的原件再生成配方。
- `cog_uint8_rgb.tif`：可从 RGBA 原件生成：

```sh
gdal_translate -of COG -b 1 -b 2 -b 3 -mask none cog_uint8_rgba.tif cog_uint8_rgb.tif
```

依赖版本以 Cargo.toml 和 Cargo.lock 为准。
