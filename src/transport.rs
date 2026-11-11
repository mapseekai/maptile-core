//! Unified payload encoding helpers.
//!
//! Tile payloads moved through `maptile-core` carry a [`TileInfo`] that
//! describes both the wire [`TileFormat`] and the compression
//! [`TileEncoding`]. Sources are not consistent about whether they return
//! compressed or uncompressed bytes, so callers (transcoders, MVT layer
//! mergers, raster pipelines) need a single place to decode and re-encode.
//!
//! This module is intentionally `http`-free: it works on `Bytes` and exposes
//! a small `decode`/`encode_as` API.

use bytes::Bytes;
use std::io::Read;
use std::sync::{Arc, OnceLock};
use tokio::sync::Semaphore;

pub use crate::sources::{TileEncoding, TileFormat, TileInfo};

use crate::error::{MaptileError, MaptileResult};

/// Maximum decoded tile payload retained by the transport layer.
pub const MAX_DECODED_TILE_BYTES: usize = 16 * 1024 * 1024;

/// Decode a payload with the default output limit. Already decoded bytes
/// share their allocation; they are checked against the same limit.
pub fn decode(data: &Bytes, encoding: TileEncoding) -> MaptileResult<Bytes> {
    decode_with_limit(data, encoding, MAX_DECODED_TILE_BYTES)
}

/// Decode a payload, rejecting output larger than `max_bytes` before it
/// can grow beyond the bound. The limit also applies to uncompressed input.
pub fn decode_with_limit(
    data: &Bytes,
    encoding: TileEncoding,
    max_bytes: usize,
) -> MaptileResult<Bytes> {
    match encoding {
        TileEncoding::Uncompressed | TileEncoding::Internal => {
            check_decoded_size(data.len(), max_bytes)?;
            Ok(data.clone())
        }
        TileEncoding::Gzip => read_decoded(flate2::read::GzDecoder::new(&data[..]), max_bytes),
        TileEncoding::Zlib => read_decoded(flate2::read::ZlibDecoder::new(&data[..]), max_bytes),
        TileEncoding::Brotli => read_decoded(brotli::Decompressor::new(&data[..], 4096), max_bytes),
        TileEncoding::Zstd => {
            let mut reader = zstd::stream::read::Decoder::new(&data[..])
                .map_err(|e| MaptileError::Transcode(format!("zstd decode: {e}")))?;
            // Bound the decoder's internal window as well as the output buffer.
            reader
                .window_log_max(24)
                .map_err(|e| MaptileError::Transcode(format!("zstd window: {e}")))?;
            read_decoded(reader, max_bytes)
        }
    }
}

fn check_decoded_size(size: usize, max_bytes: usize) -> MaptileResult<()> {
    if size > max_bytes {
        return Err(MaptileError::PayloadTooLarge { limit: max_bytes });
    }
    Ok(())
}

fn read_decoded(mut reader: impl Read, max_bytes: usize) -> MaptileResult<Bytes> {
    let mut out = Vec::new();
    reader
        .by_ref()
        .take(max_bytes as u64)
        .read_to_end(&mut out)
        .map_err(|e| MaptileError::Transcode(format!("tile decode: {e}")))?;
    let mut extra = [0_u8; 1];
    if reader
        .read(&mut extra)
        .map_err(|e| MaptileError::Transcode(format!("tile decode: {e}")))?
        != 0
    {
        return Err(MaptileError::PayloadTooLarge { limit: max_bytes });
    }
    Ok(Bytes::from(out))
}

/// Run CPU/blocking work with process-wide admission. The permit belongs
/// to the closure so cancelling its waiter does not release capacity early.
pub(crate) async fn run_blocking<T: Send + 'static>(
    work: impl FnOnce() -> MaptileResult<T> + Send + 'static,
) -> MaptileResult<T> {
    static SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    let slots = SLOTS.get_or_init(|| {
        let parallelism = std::thread::available_parallelism().map_or(1, usize::from);
        Arc::new(Semaphore::new(parallelism.clamp(1, 16)))
    });
    let permit = Arc::clone(slots)
        .acquire_owned()
        .await
        .map_err(MaptileError::backend)?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    })
    .await
    .map_err(MaptileError::backend)?
}

/// Decode on a bounded blocking worker, keeping CPU work off Tokio workers.
pub async fn decode_async(data: Bytes, encoding: TileEncoding) -> MaptileResult<Bytes> {
    if matches!(
        encoding,
        TileEncoding::Uncompressed | TileEncoding::Internal
    ) {
        check_decoded_size(data.len(), MAX_DECODED_TILE_BYTES)?;
        return Ok(data);
    }
    run_blocking(move || decode(&data, encoding)).await
}

/// Encode on a bounded blocking worker. Uncompressed output shares its input.
pub async fn encode_as_async(data: Bytes, encoding: TileEncoding) -> MaptileResult<Bytes> {
    check_decoded_size(data.len(), MAX_DECODED_TILE_BYTES)?;
    if matches!(
        encoding,
        TileEncoding::Uncompressed | TileEncoding::Internal
    ) {
        return Ok(data);
    }
    run_blocking(move || encode_as(&data, encoding)).await
}

/// Re-encode an *uncompressed* payload to the requested wire encoding. Pass
/// `TileEncoding::Uncompressed` (or `Internal`) to skip compression.
pub fn encode_as(uncompressed: &[u8], encoding: TileEncoding) -> MaptileResult<Bytes> {
    check_decoded_size(uncompressed.len(), MAX_DECODED_TILE_BYTES)?;
    match encoding {
        TileEncoding::Uncompressed | TileEncoding::Internal => {
            Ok(Bytes::copy_from_slice(uncompressed))
        }
        TileEncoding::Gzip => gzip_encode(uncompressed).map(Bytes::from),
        TileEncoding::Zlib => zlib_encode(uncompressed).map(Bytes::from),
        TileEncoding::Brotli => brotli_encode(uncompressed).map(Bytes::from),
        TileEncoding::Zstd => zstd_encode(uncompressed).map(Bytes::from),
    }
}

/// Translate a payload from one encoding to another. Decodes then re-encodes
/// only when the encodings differ.
pub fn recode(data: &Bytes, from: TileEncoding, to: TileEncoding) -> MaptileResult<Bytes> {
    if from == to {
        return Ok(data.clone());
    }
    let raw = decode(data, from)?;
    encode_as(&raw, to)
}

fn gzip_encode(data: &[u8]) -> MaptileResult<Vec<u8>> {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;
    // Level 6 — flate2's default; the standard balance between
    // compression ratio and CPU. nginx and most CDNs ship at the same
    // level. Explicit here so the choice is documented at the call
    // site rather than relying on the upstream default.
    let mut enc = GzEncoder::new(Vec::with_capacity(data.len()), Compression::new(6));
    enc.write_all(data)
        .map_err(|e| MaptileError::Transcode(format!("gzip encode: {e}")))?;
    enc.finish()
        .map_err(|e| MaptileError::Transcode(format!("gzip finish: {e}")))
}

fn zlib_encode(data: &[u8]) -> MaptileResult<Vec<u8>> {
    use flate2::Compression;
    use flate2::write::ZlibEncoder;
    use std::io::Write;
    let mut enc = ZlibEncoder::new(Vec::with_capacity(data.len()), Compression::default());
    enc.write_all(data)
        .map_err(|e| MaptileError::Transcode(format!("zlib encode: {e}")))?;
    enc.finish()
        .map_err(|e| MaptileError::Transcode(format!("zlib finish: {e}")))
}

fn brotli_encode(data: &[u8]) -> MaptileResult<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len());
    // Brotli's algorithm default is quality=11 (max). For per-request
    // tile compression that's ~10× too slow — a 3 MB MVT takes ~1 s
    // at q=11 vs ~150 ms at q=4 with only ~5–10 % size penalty. Web
    // servers (nginx, Cloudflare) typically use q=4–6 for dynamic
    // content; we match that.
    let params = brotli::enc::BrotliEncoderParams {
        quality: 4,
        ..Default::default()
    };
    let mut input = data;
    brotli::BrotliCompress(&mut input, &mut out, &params)
        .map_err(|e| MaptileError::Transcode(format!("brotli encode: {e:?}")))?;
    Ok(out)
}

fn zstd_encode(data: &[u8]) -> MaptileResult<Vec<u8>> {
    zstd::stream::encode_all(data, 0)
        .map_err(|e| MaptileError::Transcode(format!("zstd encode: {e}")))
}

/// Inspect the first few bytes for a magic-number match. Useful when a source
/// reports `TileEncoding::Uncompressed` but the payload is actually gzip
/// (common for legacy MBTiles dumps).
#[must_use]
pub fn sniff_encoding(data: &[u8]) -> Option<TileEncoding> {
    if data.len() >= 2 && data[0] == 0x1f && data[1] == 0x8b {
        return Some(TileEncoding::Gzip);
    }
    if data.len() >= 4 && data[..4] == [0x28, 0xb5, 0x2f, 0xfd] {
        return Some(TileEncoding::Zstd);
    }
    if data.len() >= 2 && data[0] == 0x78 && matches!(data[1], 0x01 | 0x5e | 0x9c | 0xda) {
        return Some(TileEncoding::Zlib);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &[u8] = b"hello, maptile-core transport!";

    #[test]
    fn all_decoders_enforce_output_limit() {
        let raw = vec![42; 64 * 1024];
        for encoding in [
            TileEncoding::Uncompressed,
            TileEncoding::Gzip,
            TileEncoding::Zlib,
            TileEncoding::Brotli,
            TileEncoding::Zstd,
        ] {
            let encoded = encode_as(&raw, encoding).unwrap();
            assert!(decode_with_limit(&encoded, encoding, raw.len() - 1).is_err());
            assert_eq!(
                decode_with_limit(&encoded, encoding, raw.len()).unwrap(),
                raw
            );
        }
    }

    #[tokio::test]
    async fn async_roundtrip_and_passthrough() {
        let raw = Bytes::from_static(SAMPLE);
        let pointer = raw.as_ptr();
        let passed = encode_as_async(raw.clone(), TileEncoding::Uncompressed)
            .await
            .unwrap();
        assert_eq!(passed.as_ptr(), pointer);
        let encoded = encode_as_async(raw.clone(), TileEncoding::Gzip)
            .await
            .unwrap();
        assert_eq!(
            decode_async(encoded, TileEncoding::Gzip).await.unwrap(),
            raw
        );
    }

    #[test]
    fn roundtrip_gzip() {
        let encoded = encode_as(SAMPLE, TileEncoding::Gzip).unwrap();
        assert_eq!(sniff_encoding(&encoded), Some(TileEncoding::Gzip));
        let decoded = decode(&encoded, TileEncoding::Gzip).unwrap();
        assert_eq!(&decoded[..], SAMPLE);
    }

    #[test]
    fn roundtrip_zlib() {
        let encoded = encode_as(SAMPLE, TileEncoding::Zlib).unwrap();
        let decoded = decode(&encoded, TileEncoding::Zlib).unwrap();
        assert_eq!(&decoded[..], SAMPLE);
    }

    #[test]
    fn roundtrip_brotli() {
        let encoded = encode_as(SAMPLE, TileEncoding::Brotli).unwrap();
        let decoded = decode(&encoded, TileEncoding::Brotli).unwrap();
        assert_eq!(&decoded[..], SAMPLE);
    }

    #[test]
    fn roundtrip_zstd() {
        let encoded = encode_as(SAMPLE, TileEncoding::Zstd).unwrap();
        assert_eq!(sniff_encoding(&encoded), Some(TileEncoding::Zstd));
        let decoded = decode(&encoded, TileEncoding::Zstd).unwrap();
        assert_eq!(&decoded[..], SAMPLE);
    }

    #[test]
    fn passthrough_uncompressed() {
        let raw = Bytes::from_static(SAMPLE);
        assert_eq!(decode(&raw, TileEncoding::Uncompressed).unwrap(), raw);
        assert_eq!(decode(&raw, TileEncoding::Internal).unwrap(), raw);
    }

    #[test]
    fn recode_short_circuit_on_match() {
        let encoded = encode_as(SAMPLE, TileEncoding::Gzip).unwrap();
        let again = recode(&encoded, TileEncoding::Gzip, TileEncoding::Gzip).unwrap();
        assert_eq!(encoded, again);
    }

    #[test]
    fn recode_gzip_to_zstd() {
        let gz = encode_as(SAMPLE, TileEncoding::Gzip).unwrap();
        let zst = recode(&gz, TileEncoding::Gzip, TileEncoding::Zstd).unwrap();
        let raw = decode(&zst, TileEncoding::Zstd).unwrap();
        assert_eq!(&raw[..], SAMPLE);
    }
}
