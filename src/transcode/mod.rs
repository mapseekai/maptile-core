//! On-the-fly tile-format transcoding.
//!
//! Currently supports the MVT ↔ MLT pair (feature flag `mlt`). The pipeline
//! is shaped so that adding new pairs (e.g. MLT → MVT2 once it exists) only
//! requires a new internal module + a `match` arm in [`transcode`].
//!
//! Source-level wire encoding (gzip / zstd / ...) is **not** handled here:
//! callers must pass uncompressed bytes. Use [`crate::transport::decode`]
//! beforehand if needed.
//!
//! Most of the heavy lifting in `mvt_mlt` (with the `mlt` feature enabled) is adapted from
//! `tileserver-rs/crates/tileserver-rs/src/transcode.rs` under the MIT license;
//! see `third_party/tileserver-rs/LICENSE`. The MVT protobuf schema is shared
//! with [`crate::sources::mvt`].

#[cfg(feature = "mlt")]
pub mod mvt_mlt;

use crate::error::{MaptileError, MaptileResult};
use crate::sources::TileFormat;
use bytes::Bytes;

/// Transcode `data` from `from` format to `to` format. Returns
/// [`MaptileError::Transcode`] for unsupported pairs.
///
/// `data` must be **uncompressed** (caller decodes any wire encoding).
pub fn transcode(data: &[u8], from: TileFormat, to: TileFormat) -> MaptileResult<Bytes> {
    if from == to {
        return Ok(Bytes::copy_from_slice(data));
    }

    match (from, to) {
        #[cfg(feature = "mlt")]
        (TileFormat::Mvt, TileFormat::Mlt) => mvt_mlt::mvt_to_mlt(data),
        #[cfg(feature = "mlt")]
        (TileFormat::Mlt, TileFormat::Mvt) => mvt_mlt::mlt_to_mvt(data),
        (from, to) => Err(MaptileError::Transcode(format!(
            "unsupported transcode: {from:?} -> {to:?}"
        ))),
    }
}

/// Like [`transcode`], but transparently decodes gzipped / brotli / zstd /
/// zlib input bytes via [`crate::transport::decode`] before transcoding.
///
/// Convenience wrapper for callers (admin endpoints, ingestion tools) that
/// haven't already separated wire encoding from tile content. Same error
/// shape as [`transcode`]; transport decode failures are surfaced as
/// `MaptileError::Transcode`.
pub fn transcode_with_gzip(data: &[u8], from: TileFormat, to: TileFormat) -> MaptileResult<Bytes> {
    let encoding = crate::transport::sniff_encoding(data)
        .unwrap_or(crate::sources::TileEncoding::Uncompressed);
    let bytes = Bytes::copy_from_slice(data);
    let decoded = crate::transport::decode(&bytes, encoding)
        .map_err(|e| MaptileError::Transcode(format!("decode wire encoding: {e}")))?;
    transcode(&decoded, from, to)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_format_is_passthrough() {
        let body = b"\x01\x02\x03";
        let out = transcode(body, TileFormat::Mvt, TileFormat::Mvt).unwrap();
        assert_eq!(&out[..], body);
    }

    #[test]
    fn unsupported_pair_returns_error() {
        let res = transcode(&[], TileFormat::Png, TileFormat::Webp);
        assert!(matches!(res, Err(MaptileError::Transcode(_))));
    }

    #[test]
    fn transcode_with_gzip_passthrough_for_uncompressed_same_format() {
        let body = b"\x01\x02\x03";
        let out = transcode_with_gzip(body, TileFormat::Mvt, TileFormat::Mvt).unwrap();
        assert_eq!(&out[..], body);
    }

    #[test]
    fn transcode_with_gzip_decodes_gzipped_input_for_same_format() {
        // Round-trip: encode, then transcode_with_gzip should decode and
        // return the original bytes for a same-format request.
        let original = b"hello world payload";
        let gz = crate::transport::encode_as(original, crate::sources::TileEncoding::Gzip).unwrap();
        let out = transcode_with_gzip(&gz, TileFormat::Mvt, TileFormat::Mvt).unwrap();
        assert_eq!(&out[..], original);
    }
}
