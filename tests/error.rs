use maptile_core::error::{MaptileError, MaptileResult};
use maptile_core::filter::FilterError;

#[test]
fn source_not_found_displays_id() {
    let e = MaptileError::SourceNotFound("nyc".into());
    assert_eq!(e.to_string(), "source not found: nyc");
}

#[test]
fn tile_not_found_displays_coords() {
    let e = MaptileError::TileNotFound(5, 10, 11);
    assert_eq!(e.to_string(), "tile not found: z=5 x=10 y=11");
}

#[test]
fn filter_error_converts_via_from() {
    let f: FilterError = FilterError::Validation("test".into());
    let e: MaptileError = f.into();
    assert!(matches!(e, MaptileError::Filter(_)));
}

#[test]
fn io_error_converts_via_from() {
    let io = std::io::Error::new(std::io::ErrorKind::NotFound, "x");
    let e: MaptileError = io.into();
    assert!(matches!(e, MaptileError::Io(_)));
}

#[test]
fn backend_wraps_arbitrary_error() {
    let inner = std::sync::Arc::new(std::io::Error::other("boom"));
    let e = MaptileError::Backend(inner);
    let msg = e.to_string();
    assert!(msg.starts_with("backend:"), "got: {msg}");
}

#[test]
fn maptile_result_alias_works() {
    fn returns_result() -> MaptileResult<u8> {
        Ok(1)
    }
    assert_eq!(returns_result().unwrap(), 1);
}
