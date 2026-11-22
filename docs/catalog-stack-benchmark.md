# Catalog tile reads over HTTP PMTiles

Warm reads through the in-process tile cache avoided all HTTP range reads in this fixture. The median per-trial p95 was 264.250 µs cold and 8.584 µs warm. Peak resident memory ranges overlapped.

## Workload

The release example creates the same 4,753,382-byte PMTiles archive for every trial. The archive contains 64 distinct 256 × 256 RGBA PNG tiles at zoom 3. An in-process HTTP/1.1 endpoint serves byte ranges from that archive over loopback. The endpoint checks the conditional validator after the first range.

Each trial reads the same 64 coordinates serially through `TileStack` with a 16 MiB tile cache and the same visibility scope. A cold trial opens the source, then reads with an empty tile cache. A warm trial primes that cache with the same coordinates before measurement. The example times each stack read and reports the nearest-rank p95. It decodes every returned PNG and checks its dimensions, format, encoding, and two coordinate-specific RGBA pixels outside the timed region.

The byte count covers HTTP response bodies for the timed sweep. The counts exclude archive open, warm priming, and HTTP headers. On macOS, `time -l` reports the process's maximum resident set size in bytes. RSS includes fixture creation, the server, the client, and PNG verification.

## Results

Five cold and five warm processes alternated on macOS 15.6.1 arm64, Mac16,10 with 10 logical CPUs and 16 GiB RAM. The 1-, 5-, and 15-minute load averages before the trials were 14.81, 19.98, and 20.16.

All trials verified 64 tiles and reported zero failures. Every cold sweep made 64 range requests and received 4,736,936 body bytes. Every warm sweep made zero range requests and received zero range body bytes.

| Trial | Cold p95 (µs) | Warm p95 (µs) | Cold peak RSS (bytes) | Warm peak RSS (bytes) |
| ---: | ---: | ---: | ---: | ---: |
| 1 | 204.333 | 12.625 | 24,739,840 | 25,526,272 |
| 2 | 274.875 | 9.708 | 26,624,000 | 28,737,536 |
| 3 | 302.041 | 5.750 | 26,984,448 | 27,082,752 |
| 4 | 264.250 | 3.250 | 27,557,888 | 27,328,512 |
| 5 | 251.959 | 8.584 | 26,820,608 | 27,607,040 |
| Median | 264.250 | 8.584 | 26,820,608 | 27,328,512 |

The counters confirm that cold reads waited for an HTTP range per tile while warm reads used cached bytes. A separate `sample` run collected no usable stack frames because the process exited during sampling. The measurement covers serial loopback reads from memory. Latency over remote storage, TLS, concurrent load, and sustained memory pressure remain unmeasured.

## Reproduce

```sh
cargo build --release --locked --features pmtiles --example catalog_stack_benchmark
for trial in 1 2 3 4 5; do
    /usr/bin/time -l ./target/release/examples/catalog_stack_benchmark cold
    /usr/bin/time -l ./target/release/examples/catalog_stack_benchmark warm
done
```

On Linux, use `/usr/bin/time -v` and convert its maximum RSS from KiB to bytes.
