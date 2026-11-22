# Catalog tile read benchmarks

## HTTP PMTiles

Warm reads through the in-process tile cache avoided all HTTP range reads in this fixture. The median per-trial p95 was 264.250 µs cold and 8.584 µs warm. Peak resident memory ranges overlapped.

### Workload

The release example creates the same 4,753,382-byte PMTiles archive for every trial. The archive contains 64 distinct 256 × 256 RGBA PNG tiles at zoom 3. An in-process HTTP/1.1 endpoint serves byte ranges from that archive over loopback. The endpoint checks the conditional validator after the first range.

Each trial reads the same 64 coordinates serially through `TileStack` with a 16 MiB tile cache and the same visibility scope. A cold trial opens the source, then reads with an empty tile cache. A warm trial primes that cache with the same coordinates before measurement. The example times each stack read and reports the nearest-rank p95. It decodes every returned PNG and checks its dimensions, format, encoding, and two coordinate-specific RGBA pixels outside the timed region.

The byte count covers HTTP response bodies for the timed sweep. The counts exclude archive open, warm priming, and HTTP headers. On macOS, `time -l` reports the process's maximum resident set size in bytes. RSS includes fixture creation, the server, the client, and PNG verification.

### Results

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

### Reproduce

```sh
cargo build --release --locked --features pmtiles --example catalog_stack_benchmark
for trial in 1 2 3 4 5; do
    /usr/bin/time -l ./target/release/examples/catalog_stack_benchmark cold
    /usr/bin/time -l ./target/release/examples/catalog_stack_benchmark warm
done
```

On Linux, use `/usr/bin/time -v` and convert its maximum RSS from KiB to bytes.

## Online reads during a large export

Five paired release runs compared commit `0b76c4c` with the final implementation, where pinned exports reuse online hits and skip admission on export-only misses. Each run used a fresh process, a 1 MiB cache, and 112 prewarmed tiles of about 8 KiB each. Of 1,536 online requests, 1,152 rotated over 16 frequently accessed tiles and 384 over the other 96. Eight export sessions read 8,192 distinct tiles from a separate row of the same source. The synthetic source delayed misses by 2 ms. The probe checked every response against its coordinate-specific MVT fingerprint.

| During or after export | Baseline median (range) | Final median (range) |
| --- | ---: | ---: |
| Backend reads for the 384 less frequent requests | 381 (378–381) | 0 (0–0) |
| Less frequent online p95 | 3.977 ms (3.708–5.835) | 0.026 ms (0.018–0.034) |
| Backend reads for the 1,152 frequent requests | 0 (0–0) | 0 (0–0) |
| Frequent online p95 | 12.08 µs (8.04–13.38) | 18.46 µs (16.17–22.00) |
| Backend reads on the first 112 online reads after export | 112 (112–112) | 0 (0–0) |
| Longest export worker duration (1,024 tiles per worker) | 3.492 s (3.421–3.836) | 3.419 s (3.382–3.573) |

Both versions issued exactly one source read per exported coordinate. Without an export, every online request hit the cache on both versions. The latency figures include local scheduling and the synthetic miss delay. The backend counters directly measure eviction in this workload.

The mixed-load probe issues online requests serially at a 1 ms interval and times each read after its tick. A separate unpaced hot-hit probe ran 90,000 reads of 16 prewarmed keys per process. Across ten processes per implementation, median p95 was 792 ns at baseline and 625 ns with the cache-miss future boxed after the first lookup. Each process made exactly 16 source reads during warm-up.

Use the same probe source in both checkouts and a separate Cargo target directory for each. Alternate release binaries over at least five rounds. [Export probe](../examples/export_cache_pressure.rs)

```sh
cargo run --release --locked --example export_cache_pressure -- control
cargo run --release --locked --example export_cache_pressure -- export
```

## Multi-member MVT composition

The composition probe reads through `TileStack` with the tile cache disabled. Each member supplies a distinct MVT layer containing 2,400 line features with geometry and attributes. Each process measures 96 coordinates per case after warming up. It verifies every result against the expected bytes, decodes sampled results, and checks that each member served each coordinate exactly once. Setup and output verification are outside the timed interval. [Composition probe](../examples/composite_decode_pressure.rs)

The table shows the median of six per-process p95 values and their range. Three release binaries ran in all six orders on the same 10-core arm64 Mac using Rust 1.98.1. Each version had a separate Cargo target directory. The baseline held the request's decode lock through layer validation. The final version releases it after decompression, byte accounting and disposal of the encoded input. Each leaf retains its concurrency permit until the blocking job exits, including when a nested caller cancels. All 20,736 measured requests and 89,856 leaf reads passed output and read-count checks.

| Encoding | Members | Concurrent requests | Baseline p95 in ms | Narrower lock p95 in ms |
| --- | ---: | ---: | ---: | ---: |
| Gzip | 1 | 1 | 0.865 (0.538–6.868) | 1.030 (0.810–3.255) |
| Gzip | 1 | 8 | 2.169 (1.685–3.559) | 2.213 (1.844–6.904) |
| Gzip | 4 | 1 | 2.654 (2.088–17.257) | 2.431 (1.089–8.276) |
| Gzip | 4 | 8 | 7.187 (4.679–30.810) | 11.678 (4.424–32.300) |
| Gzip | 8 | 1 | 7.019 (4.448–32.421) | 2.295 (1.956–5.675) |
| Gzip | 8 | 8 | 20.973 (14.313–95.289) | 14.442 (11.108–40.352) |
| Raw | 1 | 1 | 0.375 (0.347–2.248) | 0.523 (0.351–2.929) |
| Raw | 1 | 8 | 2.191 (1.652–11.874) | 2.265 (1.722–9.006) |
| Raw | 4 | 1 | 1.404 (1.304–12.091) | 0.664 (0.430–4.662) |
| Raw | 4 | 8 | 4.504 (3.832–14.628) | 6.388 (4.942–23.170) |
| Raw | 8 | 1 | 3.009 (2.742–19.238) | 1.195 (0.870–33.092) |
| Raw | 8 | 8 | 12.061 (9.778–85.796) | 13.915 (10.787–34.740) |

In a separate instrumented baseline run with eight gzip members and one caller, the lock was held for 4.00 ms per request in total. Decompression accounted for 1.42 ms and layer validation for 2.58 ms. The sum of the eight members' lock waits was 13.83 ms because some waits overlapped. The lock is local to one request and its nested members. Single-member composition spent about 0.002 ms in the final compose step in the gzip case.

The third binary decoded compressed members concurrently and charged retained output in chunks. Its eight-member gzip p95 was 1.785 ms with one caller and 10.842 ms with eight callers. The selected version keeps one active codec workspace per composite request. It reduced eight-member gzip latency at both concurrency levels. Four-member gzip and raw p95 increased with eight callers. A separate two-member test passed all 96 reads at a 341,180-byte working limit and rejected all 96 when the limit fell by one byte. The fixtures use in-memory drivers, so these results cover composition and scheduling on this host.

Five additional paired processes ran the gzip cases with 16 requests per case. Median peak RSS was 60.67 MiB (58.41–62.42) at baseline and 63.86 MiB (58.94–64.47) with concurrent layer validation. These process measurements include fixtures, retained results, and temporary parser allocations. `max_work_bytes` accounts for reservations for tile payload buffers.

A separate counting allocator measured the peak increase in live Rust heap during one warmed, eight-member read. The payload budget was 1,364,744 bytes and each output was 682,372 bytes. Across five processes per version and 16 reads per process, the median of the per-process medians rose from 0.84 to 4.73 MiB for raw tiles and from 1.82 to 3.14 MiB for gzip. Concurrent validators can hold several members' feature and value metadata at once. Setting `CompositeLimits::max_concurrency` to 1 serializes leaf reads and validation.

```sh
cargo run --release --locked --example composite_decode_pressure -- 96 2400 both
```
