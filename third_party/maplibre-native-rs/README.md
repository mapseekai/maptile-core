# MapLibre Native Rust binding

This directory contains the build inputs from the published `maplibre_native`
0.9.3 crate. `maptile-core` uses this relative path dependency so the native ABI
fixes also reach applications that depend on this repository by Git or path.
It is not a separately published binding release.

## Origin and license

- Upstream: <https://github.com/maplibre/maplibre-native-rs>
- Crate: <https://crates.io/crates/maplibre_native/0.9.3>
- Published archive SHA-256:
  `d446b3c5a43e227d3ace7c70739252362ae70e3571548063d434f0c2368d3267`
- Archive VCS revision: `2cf177200470f80a2674fa5870e122ee18e6c157`
- Binding license: [MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE).
  Upstream authors and source attribution are retained.

The Rust/C++ binding sources are unchanged. The manifest retains the package
version, runtime dependencies and features, removes upstream test targets and
unused download/manifest-parser build dependencies, and disables publishing.
Upstream tests, examples, workflows and development dependencies are omitted.

## Local build changes

`build.rs` pins Native to `a33d3f00fb9dbf9117ce18edd799aa0ca5bcce5b`, enables
RTTI in both core and bridge, and matches the bridge's `NDEBUG` to the core's
Debug or optimized build. The managed source checkout lives under Cargo's
`OUT_DIR`; the dependency source directory need not be writable. Explicit
`MLN_LOCAL_REPOSITORY` source checkouts remain supported.

Precompiled builds require explicit, matching archive and header inputs with
RTTI enabled and `NDEBUG` defined. There is no automatic download: the original
0.9.3 assets do not meet this ABI contract. Header refresh removes stale generated
headers while checking that cleanup cannot sever the input paths, including
symlinks. Checks use a fresh backup directory so an interrupted earlier check
does not block subsequent builds. A forced kill can leave a complete backup and
an inaccessible input alias; the build log prints the backup and restore paths.
Restore that backup before retrying such an alias, and do not delete it blindly.

For prerequisites, ordinary Cargo commands and both build modes, see
[the project build guide](../../docs/development.md#原生渲染构建). Native itself
and its dependencies retain their own licenses; distribute the generated
`mbgl-core.license` with native binaries.
