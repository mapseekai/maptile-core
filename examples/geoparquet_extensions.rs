//! Explicit deployment/CI preparation for the embedded DuckDB version.
//! The tile server never calls this installer on a request path.

use duckdb::{Config, Connection};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let install = match args.as_slice() {
        [] => false,
        [flag] if flag == "--check" => false,
        [flag] if flag == "--install" => true,
        [flag] if flag == "--help" => {
            println!("geoparquet_extensions [--check | --install]");
            println!("Default: check the local Spatial/HTTPFS extensions without network access.");
            println!(
                "--install: install the signed extension from DuckDB's official core repository."
            );
            return Ok(());
        }
        _ => return Err("usage: geoparquet_extensions [--check | --install]".into()),
    };
    let config = Config::default().enable_autoload_extension(false)?;
    let conn = Connection::open_in_memory_with_flags(config)?;
    let version: String = conn.query_row("SELECT version()", [], |row| row.get(0))?;
    let platform: String = conn.query_row("PRAGMA platform", [], |row| row.get(0))?;
    println!("Embedded DuckDB {version}, platform {platform}");
    if install {
        // INSTALL selects the embedded version/platform, not a system CLI's
        // version. Only the official core repository is used; DuckDB verifies
        // the signature when loading. No unsigned-extension option is enabled.
        conn.execute_batch("INSTALL spatial FROM core; INSTALL httpfs FROM core")?;
    }
    conn.execute_batch("LOAD parquet; LOAD spatial; LOAD httpfs")?;
    println!("Parquet, Spatial and HTTPFS extensions are available for this service user.");
    Ok(())
}
