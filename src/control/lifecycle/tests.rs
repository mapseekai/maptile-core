#[cfg(all(unix, target_pointer_width = "64"))]
#[test]
fn cleanup_worker_creation_failure_returns_an_error() {
    let thread = std::thread::Builder::new().stack_size(1 << 60);
    assert!(super::start_cleanup_runtime(thread).is_err());
}
