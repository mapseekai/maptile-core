#!/usr/bin/env bash

set -euo pipefail

features=${1:-}
case ",$features," in
    ,,)
        forbidden='(zstd-sys|libsqlite3-sys|sqlx-sqlite|libduckdb-sys|duckdb|tokio-postgres|deadpool-postgres|postgres-types|proj-sys|gdal-sys|maplibre_native|maplibre_native_sys)'
        libraries='lib(gdal|proj|sqlite3|zstd|duckdb|pq|maplibre)'
        ;;
    ,parquet-archive,)
        forbidden='(libduckdb-sys|duckdb|zstd-sys)'
        libraries='lib(duckdb|zstd)'
        ;;
    ,agtiff,|,stac,)
        forbidden='(gdal|gdal-sys)'
        libraries='libgdal'
        ;;
    *) exit 0 ;;
esac

tree=$(cargo tree --locked --no-default-features --features "$features" -e normal --prefix none)
if matches=$(printf '%s\n' "$tree" | grep -E "^${forbidden} v"); then
    printf '%s\n' "$matches" >&2
    exit 1
fi

executable=$(
    cargo test --locked --no-default-features --features "$features" --lib --no-run --message-format=json |
    python3 -c 'import json, sys
for line in sys.stdin:
    event = json.loads(line)
    if event.get("executable"):
        print(event["executable"])'
)
case $(uname -s) in
    Darwin) linked=$(otool -L "$executable") ;;
    Linux) linked=$(ldd "$executable") ;;
    *) exit 0 ;;
esac
if matches=$(printf '%s\n' "$linked" | grep -E "$libraries"); then
    printf '%s\n' "$matches" >&2
    exit 1
fi
