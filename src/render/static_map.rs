//! Static-map URL parameter parsing.
//!
//! Mapbox-style static endpoints encode the viewport in the URL path:
//!
//! ```text
//! /styles/{id}/static/{lon},{lat},{zoom}/{w}x{h}.png       — center
//! /styles/{id}/static/{minx},{miny},{maxx},{maxy}/{w}x{h}.png — bbox
//! /styles/{id}/static/auto/{w}x{h}.png                      — auto-fit
//! ```
//!
//! This module parses the centre/bbox/auto string and converts to a
//! [`crate::render::RenderRequest`] (or returns the bbox for an external
//! auto-fit pass that consumes overlays).

use crate::render::types::{RenderCamera, RenderFormat, RenderRequest};
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq)]
pub enum StaticTarget {
    Center {
        lon: f64,
        lat: f64,
        zoom: f64,
        bearing: f64,
        pitch: f64,
    },
    BoundingBox {
        min_lon: f64,
        min_lat: f64,
        max_lon: f64,
        max_lat: f64,
    },
    Auto,
}

impl FromStr for StaticTarget {
    type Err = StaticTargetError;

    /// Parse the `{spec}` segment of a static map URL.
    ///
    /// Examples:
    /// - `-122.4,37.8,12` → `Center`
    /// - `-122.4,37.8,12@45` → `Center` with bearing
    /// - `-122.4,37.8,12@45,60` → `Center` with bearing + pitch
    /// - `-123,37,-122,38` → `BoundingBox`
    /// - `auto` → `Auto`
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "auto" {
            return Ok(Self::Auto);
        }

        let parts: Vec<&str> = s.split(',').collect();

        // Center with explicit pitch: "lon,lat,zoom@bearing,pitch" (4 commas, 5 parts? no — 4)
        // We disambiguate from BoundingBox by checking for '@' in parts[2].
        if parts.len() == 4 && parts[2].contains('@') {
            let lon: f64 = parts[0]
                .parse()
                .map_err(|_| StaticTargetError::InvalidField("lon"))?;
            let lat: f64 = parts[1]
                .parse()
                .map_err(|_| StaticTargetError::InvalidField("lat"))?;
            let (zoom_str, bearing_str) = parts[2]
                .split_once('@')
                .ok_or(StaticTargetError::InvalidField("zoom"))?;
            let zoom: f64 = zoom_str
                .parse()
                .map_err(|_| StaticTargetError::InvalidField("zoom"))?;
            let bearing: f64 = bearing_str
                .parse()
                .map_err(|_| StaticTargetError::InvalidField("bearing"))?;
            let pitch: f64 = parts[3]
                .parse()
                .map_err(|_| StaticTargetError::InvalidField("pitch"))?;
            return Ok(Self::Center {
                lon,
                lat,
                zoom,
                bearing,
                pitch,
            });
        }

        if parts.len() == 4 {
            let parse = |i: usize, name: &'static str| -> Result<f64, StaticTargetError> {
                parts[i]
                    .parse()
                    .map_err(|_| StaticTargetError::InvalidField(name))
            };
            return Ok(Self::BoundingBox {
                min_lon: parse(0, "min_lon")?,
                min_lat: parse(1, "min_lat")?,
                max_lon: parse(2, "max_lon")?,
                max_lat: parse(3, "max_lat")?,
            });
        }

        if parts.len() == 3 {
            let lon: f64 = parts[0]
                .parse()
                .map_err(|_| StaticTargetError::InvalidField("lon"))?;
            let lat: f64 = parts[1]
                .parse()
                .map_err(|_| StaticTargetError::InvalidField("lat"))?;
            let (zoom_str, bearing_str) = match parts[2].split_once('@') {
                Some((z, rest)) => (z, Some(rest)),
                None => (parts[2], None),
            };
            let zoom: f64 = zoom_str
                .parse()
                .map_err(|_| StaticTargetError::InvalidField("zoom"))?;

            let bearing = match bearing_str {
                None => 0.0,
                Some(rest) => rest
                    .parse::<f64>()
                    .map_err(|_| StaticTargetError::InvalidField("bearing"))?,
            };

            return Ok(Self::Center {
                lon,
                lat,
                zoom,
                bearing,
                pitch: 0.0,
            });
        }

        Err(StaticTargetError::Unrecognised(s.to_string()))
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum StaticTargetError {
    #[error("invalid {0} component")]
    InvalidField(&'static str),
    #[error("unrecognised static target: {0}")]
    Unrecognised(String),
}

impl From<StaticTargetError> for crate::error::MaptileError {
    fn from(value: StaticTargetError) -> Self {
        crate::error::MaptileError::Render(value.to_string())
    }
}

impl StaticTarget {
    /// Convert to a [`RenderRequest`]. For [`StaticTarget::BoundingBox`],
    /// computes a centre + zoom that fits the bbox into the requested
    /// viewport. For [`StaticTarget::Auto`] returns
    /// `Err(StaticTargetError::Unrecognised)` — auto-fit needs overlay
    /// inputs that callers provide externally.
    pub fn into_request(
        self,
        style_json: impl Into<String>,
        width: u32,
        height: u32,
        scale: u8,
        format: RenderFormat,
    ) -> Result<RenderRequest, StaticTargetError> {
        let camera = match self {
            Self::Center {
                lon,
                lat,
                zoom,
                bearing,
                pitch,
            } => RenderCamera {
                lon,
                lat,
                zoom,
                bearing,
                pitch,
            },
            Self::BoundingBox {
                min_lon,
                min_lat,
                max_lon,
                max_lat,
            } => {
                let lon = (min_lon + max_lon) / 2.0;
                let lat = (min_lat + max_lat) / 2.0;
                let zoom = bbox_to_zoom(min_lon, min_lat, max_lon, max_lat, width, height);
                RenderCamera::new(lon, lat, zoom)
            }
            Self::Auto => return Err(StaticTargetError::Unrecognised("auto".into())),
        };
        Ok(RenderRequest {
            isolate_native_state: false,
            style_json: style_json.into(),
            camera,
            width,
            height,
            scale,
            format,
            #[cfg(feature = "render")]
            overlay_items: None,
        })
    }
}

/// Estimate zoom that fits a longitude/latitude span into a pixel viewport,
/// assuming Web Mercator and 512px tiles. Conservative — the renderer can
/// still clamp to its own min/max zoom.
fn bbox_to_zoom(
    min_lon: f64,
    min_lat: f64,
    max_lon: f64,
    max_lat: f64,
    width: u32,
    height: u32,
) -> f64 {
    const WORLD_PX_AT_Z0: f64 = 512.0;

    let lon_span = (max_lon - min_lon).abs().max(1e-9);
    let zoom_lon = (WORLD_PX_AT_Z0 * (width as f64 / 360.0) / lon_span).log2();

    // Latitude span via Mercator projection y values.
    let to_y = |lat: f64| {
        let r = lat.to_radians();
        ((1.0 + r.sin()) / (1.0 - r.sin())).ln() / 2.0
    };
    let lat_span = (to_y(max_lat) - to_y(min_lat)).abs().max(1e-9);
    let zoom_lat =
        (WORLD_PX_AT_Z0 * (height as f64 / (2.0 * std::f64::consts::PI)) / lat_span).log2();

    zoom_lon.min(zoom_lat).clamp(0.0, 22.0)
}

/// Convenience: build a [`RenderRequest`] from a parsed static target plus
/// validation. Returns the same `MaptileError` chain as the rest of core.
pub fn build_request(
    target: StaticTarget,
    style_json: impl Into<String>,
    width: u32,
    height: u32,
    scale: u8,
    format: RenderFormat,
) -> crate::error::MaptileResult<RenderRequest> {
    let req = target.into_request(style_json, width, height, scale, format)?;
    let req = req.validate().map_err(crate::error::MaptileError::from)?;
    Ok(req)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_center_with_bearing_and_pitch() {
        let t: StaticTarget = "-122.4,37.8,12@45,60".parse().unwrap();
        match t {
            StaticTarget::Center {
                lon,
                lat,
                zoom,
                bearing,
                pitch,
            } => {
                assert!((lon + 122.4).abs() < 1e-9);
                assert!((lat - 37.8).abs() < 1e-9);
                assert_eq!(zoom, 12.0);
                assert_eq!(bearing, 45.0);
                assert_eq!(pitch, 60.0);
            }
            _ => panic!("expected Center"),
        }
    }

    #[test]
    fn parses_bbox() {
        let t: StaticTarget = "-123,37,-122,38".parse().unwrap();
        match t {
            StaticTarget::BoundingBox {
                min_lon,
                min_lat,
                max_lon,
                max_lat,
            } => {
                assert_eq!(
                    (min_lon, min_lat, max_lon, max_lat),
                    (-123.0, 37.0, -122.0, 38.0)
                );
            }
            _ => panic!("expected BoundingBox"),
        }
    }

    #[test]
    fn parses_auto() {
        let t: StaticTarget = "auto".parse().unwrap();
        assert_eq!(t, StaticTarget::Auto);
    }

    #[test]
    fn rejects_garbage() {
        assert!("hello".parse::<StaticTarget>().is_err());
        assert!("1,2".parse::<StaticTarget>().is_err());
        assert!("a,b,c".parse::<StaticTarget>().is_err());
    }

    #[test]
    fn bbox_yields_finite_zoom() {
        let z = bbox_to_zoom(-123.0, 37.0, -122.0, 38.0, 800, 600);
        assert!(z.is_finite());
        assert!(z > 0.0 && z < 22.0);
    }

    #[test]
    fn bbox_into_request_centers_correctly() {
        let target = StaticTarget::BoundingBox {
            min_lon: -10.0,
            min_lat: -5.0,
            max_lon: 10.0,
            max_lat: 5.0,
        };
        let req = target
            .into_request("{}", 512, 512, 1, RenderFormat::Png)
            .unwrap();
        assert!(req.camera.lon.abs() < 1e-9);
        assert!(req.camera.lat.abs() < 1e-9);
    }

    #[test]
    fn build_request_validates() {
        let target = StaticTarget::Center {
            lon: 0.0,
            lat: 0.0,
            zoom: 5.0,
            bearing: 0.0,
            pitch: 0.0,
        };
        let req = build_request(target, "{}", 512, 512, 1, RenderFormat::Png).unwrap();
        assert_eq!(req.width, 512);
        // Oversized rejection
        let target2 = StaticTarget::Center {
            lon: 0.0,
            lat: 0.0,
            zoom: 5.0,
            bearing: 0.0,
            pitch: 0.0,
        };
        let res = build_request(target2, "{}", 99_999, 99_999, 1, RenderFormat::Png);
        assert!(res.is_err());
    }
}
