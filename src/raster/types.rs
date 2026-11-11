//! Resampling-method config for raster reprojection / overview reads.
//!
//! Mirrors GDAL's `ResampleAlg` so the value can be parsed from
//! configuration without exposing GDAL types in the public API.

use serde::{Deserialize, Serialize};

#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ResamplingMethod {
    Nearest,
    #[default]
    Bilinear,
    Cubic,
    CubicSpline,
    Lanczos,
    Average,
    Mode,
}

impl std::fmt::Display for ResamplingMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Nearest => write!(f, "nearest"),
            Self::Bilinear => write!(f, "bilinear"),
            Self::Cubic => write!(f, "cubic"),
            Self::CubicSpline => write!(f, "cubic_spline"),
            Self::Lanczos => write!(f, "lanczos"),
            Self::Average => write!(f, "average"),
            Self::Mode => write!(f, "mode"),
        }
    }
}

impl std::str::FromStr for ResamplingMethod {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "nearest" => Ok(Self::Nearest),
            "bilinear" => Ok(Self::Bilinear),
            "cubic" => Ok(Self::Cubic),
            "cubic_spline" => Ok(Self::CubicSpline),
            "lanczos" => Ok(Self::Lanczos),
            "average" => Ok(Self::Average),
            "mode" => Ok(Self::Mode),
            _ => Err(format!("unknown resampling method: {s}")),
        }
    }
}

#[cfg(feature = "raster")]
impl From<ResamplingMethod> for gdal::raster::ResampleAlg {
    fn from(method: ResamplingMethod) -> Self {
        match method {
            ResamplingMethod::Nearest => Self::NearestNeighbour,
            ResamplingMethod::Bilinear => Self::Bilinear,
            ResamplingMethod::Cubic => Self::Cubic,
            ResamplingMethod::CubicSpline => Self::CubicSpline,
            ResamplingMethod::Lanczos => Self::Lanczos,
            ResamplingMethod::Average => Self::Average,
            ResamplingMethod::Mode => Self::Mode,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn round_trip_display_parse() {
        for m in [
            ResamplingMethod::Nearest,
            ResamplingMethod::Bilinear,
            ResamplingMethod::Cubic,
            ResamplingMethod::CubicSpline,
            ResamplingMethod::Lanczos,
            ResamplingMethod::Average,
            ResamplingMethod::Mode,
        ] {
            assert_eq!(ResamplingMethod::from_str(&m.to_string()).unwrap(), m);
        }
    }

    #[test]
    fn parse_is_case_insensitive() {
        assert_eq!(
            ResamplingMethod::from_str("LANCZOS").unwrap(),
            ResamplingMethod::Lanczos
        );
    }

    #[test]
    fn rejects_unknown_method() {
        assert!(ResamplingMethod::from_str("triangle").is_err());
    }

    #[test]
    fn default_is_bilinear() {
        assert_eq!(ResamplingMethod::default(), ResamplingMethod::Bilinear);
    }
}
