#!/usr/bin/env bash

set -euo pipefail

commit="$(printenv MAPTILE_MLN_COMMIT || printf '%s' a33d3f00fb9dbf9117ce18edd799aa0ca5bcce5b)"
source_url="$(printenv MAPTILE_MLN_SOURCE_URL || printf '%s' https://github.com/maplibre/maplibre-native.git)"
checkout="$(printenv MAPTILE_MLN_CHECKOUT || printf '%s' "$PWD/.cache/maplibre-native-src/maplibre-native")"

case "$checkout" in
  */maplibre-native) ;;
  *)
    printf '%s\n' "checkout must end in /maplibre-native" >&2
    exit 2
    ;;
esac

if [ ! -d "$checkout/.git" ]; then
  git init --quiet "$checkout"
  git -C "$checkout" remote add origin "$source_url"
fi

if ! revision="$(git -C "$checkout" rev-parse --verify "$commit^{commit}" 2>/dev/null)"; then
  git -C "$checkout" fetch --depth 1 origin "$commit"
  revision=FETCH_HEAD
fi
git -C "$checkout" checkout --detach --force "$revision"
git -C "$checkout" submodule sync --recursive
git -C "$checkout" submodule update --init --recursive --force --depth 1
test -f "$checkout/vendor/boost/LICENSE_1_0.txt"
test -f "$checkout/scripts/license.cmake"
test -z "$(git -C "$checkout" submodule status --recursive | sed -n '/^[-+U]/p')"

if [ "$#" -gt 0 ] && [ "$1" = "--prepare-only" ]; then
  exit 0
fi

# This runner verifies source builds even when the caller normally uses precompiled cores.
unset DOCS_RS MLN_CORE_LIBRARY_PATH MLN_CORE_LIBRARY_HEADERS_PATH
MLN_PRECOMPILE=0 MLN_SYSTEM=0 MLN_LOCAL_REPOSITORY="$checkout" cargo "$@"
