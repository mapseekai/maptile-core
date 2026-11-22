//! Read XYZ from a PMTiles archive. Optionally write the original bytes to a file.
mod common;
use maptile_core::prelude::*;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let mut path = common::fixture();
    let mut coord = TileCoord { z: 0, x: 0, y: 0 };
    if let [archive, z, x, y, ..] = args.as_slice()
        && args.len() <= 5
    {
        path = archive.into();
        coord = TileCoord {
            z: z.to_str().ok_or("invalid zoom")?.parse()?,
            x: x.to_str().ok_or("invalid x")?.parse()?,
            y: y.to_str().ok_or("invalid y")?.parse()?,
        };
    } else if !args.is_empty() {
        return Err("usage: read_pmtiles [archive.pmtiles z x y [output]]".into());
    }
    let stack = TileStack::builder(common::catalog(&path).await?).build();
    let result = stack
        .read(ReadTarget::new("sample", "cli").at(coord))
        .await?;
    let Some(tile) = result.tile() else {
        return Err("no tile at requested coordinate".into());
    };
    if let Some(output) = args.get(4) {
        tokio::fs::write(PathBuf::from(output), &tile.data).await?;
    }
    println!(
        "sample {}/{}/{}: {} bytes ({:?}, {:?})",
        coord.z,
        coord.x,
        coord.y,
        tile.data.len(),
        tile.info.format,
        tile.info.encoding
    );
    Ok(())
}
