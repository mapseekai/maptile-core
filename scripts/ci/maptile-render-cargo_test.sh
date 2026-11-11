#!/usr/bin/env bash

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
work="$(mktemp -d)"
trap 'status=$?; rm -rf "$work"; exit "$status"' EXIT

git_config="$work/gitconfig"
export GIT_CONFIG_GLOBAL="$git_config"
git config --global protocol.file.allow always

boost_repo="$work/boost"
fixture_repo="$work/maplibre-fixture"

mkdir -p "$boost_repo"
git -C "$boost_repo" init --quiet
printf '%s\n' "Boost Software License - Version 1.0" > "$boost_repo/LICENSE_1_0.txt"
git -C "$boost_repo" add LICENSE_1_0.txt
git -C "$boost_repo" -c user.name="Maptile Core CI" -c user.email="ci@maptile-core.local" commit --quiet -m "Add Boost license"

mkdir -p "$fixture_repo/scripts"
git -C "$fixture_repo" init --quiet
printf '%s\n' "# fixture license collector" > "$fixture_repo/scripts/license.cmake"
git -C "$fixture_repo" add scripts/license.cmake
git -C "$fixture_repo" -c user.name="Maptile Core CI" -c user.email="ci@maptile-core.local" commit --quiet -m "Add license collector"
git -C "$fixture_repo" submodule add --quiet "$boost_repo" vendor/boost
git -C "$fixture_repo" -c user.name="Maptile Core CI" -c user.email="ci@maptile-core.local" commit --quiet -m "Add Boost submodule"
fixture_commit="$(git -C "$fixture_repo" rev-parse HEAD)"

git clone --quiet "$fixture_repo" "$work/maplibre-native"
git -C "$work/maplibre-native" submodule update --init --recursive --quiet
rm "$work/maplibre-native/vendor/boost/LICENSE_1_0.txt"
# A verified cached commit must not require the source remote to be reachable.
git -C "$work/maplibre-native" remote set-url origin "$work/unavailable-origin"

set +e
MAPTILE_MLN_SOURCE_URL="$fixture_repo" \
MAPTILE_MLN_COMMIT="$fixture_commit" \
MAPTILE_MLN_CHECKOUT="$work/maplibre-native" \
  "$repo_root/scripts/ci/maptile-render-cargo.sh" --prepare-only
status=$?
set -e
[ "$status" -eq 0 ] || exit "$status"

test -f "$work/maplibre-native/vendor/boost/LICENSE_1_0.txt"
test -z "$(git -C "$work/maplibre-native" submodule status --recursive | sed -n '/^[-+U]/p')"

mkdir -p "$work/bin"
cat > "$work/bin/cargo" <<'CARGO'
#!/usr/bin/env bash
set -euo pipefail
test "$MLN_PRECOMPILE" = 0
test "$MLN_SYSTEM" = 0
test "${DOCS_RS+x}" != x
test "${MLN_CORE_LIBRARY_PATH+x}" != x
test "${MLN_CORE_LIBRARY_HEADERS_PATH+x}" != x
test -f "$MLN_LOCAL_REPOSITORY/vendor/boost/LICENSE_1_0.txt"
test "$(git -C "$MLN_LOCAL_REPOSITORY" rev-parse HEAD)" = "$MAPTILE_MLN_COMMIT"
test "$*" = "build --locked -p maptile-core --features render"
printf '%s\n' "source cargo invoked" > "$MAPTILE_TEST_MARKER"
CARGO
chmod +x "$work/bin/cargo"

PATH="$work/bin:$PATH" \
DOCS_RS=1 MLN_PRECOMPILE=1 MLN_SYSTEM=1 \
MLN_CORE_LIBRARY_PATH=/unused/precompiled.a \
MLN_CORE_LIBRARY_HEADERS_PATH=/unused/headers.tar.gz \
MAPTILE_MLN_SOURCE_URL="$fixture_repo" \
MAPTILE_MLN_COMMIT="$fixture_commit" \
MAPTILE_MLN_CHECKOUT="$work/fresh/maplibre-native" \
MAPTILE_TEST_MARKER="$work/cargo-invoked" \
  "$repo_root/scripts/ci/maptile-render-cargo.sh" build --locked -p maptile-core --features render

test "$(cat "$work/cargo-invoked")" = "source cargo invoked"
