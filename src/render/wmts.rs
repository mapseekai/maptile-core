//! WMTS 1.0.0 GetCapabilities XML builder.
//!
//! Produces the XML payload served at `/styles/{id}/wmts.xml` so that
//! desktop GIS clients (QGIS, ArcGIS) can register the tileset. The
//! function is pure — it takes a description and returns a `String`.

use quick_xml::Writer;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};

/// One layer (style) advertised in the WMTS document.
#[derive(Debug, Clone)]
pub struct WmtsLayer {
    pub identifier: String,
    pub title: String,
    pub abstract_: Option<String>,
    /// Tile URL template, e.g.
    /// `https://example.com/styles/bright/{TileMatrix}/{TileCol}/{TileRow}.png`.
    pub tile_url_template: String,
    /// Image MIME type, e.g. `image/png`.
    pub format: String,
    /// Optional WGS84 bounding box: `[west, south, east, north]`.
    pub bounding_box: Option<[f64; 4]>,
    pub min_zoom: u8,
    pub max_zoom: u8,
}

#[derive(Debug, Clone)]
pub struct WmtsService {
    pub title: String,
    pub abstract_: Option<String>,
    /// Public URL of the WMTS endpoint itself (used as the `OperationsMetadata` href).
    pub service_url: String,
}

/// Build the WMTS 1.0.0 GetCapabilities XML.
///
/// Generates a single `GoogleMapsCompatible`-style TileMatrixSet covering
/// zooms 0..=22 (the renderer's effective range is constrained per layer
/// via `min_zoom`/`max_zoom`). MIME-types are emitted verbatim.
///
/// Implementation note: the document is emitted via `quick-xml`'s indented
/// writer (2-space indent) which handles all element/attribute escaping.
/// Output uses one element per line — there is no longer any
/// hand-collapsed inline form.
#[must_use]
pub fn build_capabilities(service: &WmtsService, layers: &[WmtsLayer]) -> String {
    let mut writer = Writer::new_with_indent(Vec::with_capacity(4096), b' ', 2);

    // <?xml version="1.0" encoding="UTF-8"?>
    writer
        .write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .expect("infallible write to Vec<u8>");

    // <Capabilities ...>
    let mut caps = BytesStart::new("Capabilities");
    caps.push_attribute(("xmlns", "http://www.opengis.net/wmts/1.0"));
    caps.push_attribute(("xmlns:ows", "http://www.opengis.net/ows/1.1"));
    caps.push_attribute(("xmlns:xlink", "http://www.w3.org/1999/xlink"));
    caps.push_attribute(("xmlns:gml", "http://www.opengis.net/gml"));
    caps.push_attribute(("version", "1.0.0"));
    write_event(&mut writer, Event::Start(caps));

    write_service_identification(&mut writer, service);
    write_operations_metadata(&mut writer, service);
    write_contents(&mut writer, layers);

    write_event(&mut writer, Event::End(BytesEnd::new("Capabilities")));

    let mut out = String::from_utf8(writer.into_inner())
        .expect("quick-xml emits valid UTF-8 by construction");
    // Match the historical builder which terminated with a trailing newline.
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Write an event, panicking on the (infallible) `Vec<u8>` write path.
fn write_event(writer: &mut Writer<Vec<u8>>, event: Event<'_>) {
    writer
        .write_event(event)
        .expect("infallible write to Vec<u8>");
}

/// Emit `<tag>text</tag>` (text is escaped by `BytesText::new`).
fn write_text_element(writer: &mut Writer<Vec<u8>>, tag: &str, text: &str) {
    write_event(writer, Event::Start(BytesStart::new(tag)));
    write_event(writer, Event::Text(BytesText::new(text)));
    write_event(writer, Event::End(BytesEnd::new(tag)));
}

fn write_service_identification(writer: &mut Writer<Vec<u8>>, service: &WmtsService) {
    write_event(
        writer,
        Event::Start(BytesStart::new("ows:ServiceIdentification")),
    );
    write_text_element(writer, "ows:Title", &service.title);
    if let Some(abs) = &service.abstract_ {
        write_text_element(writer, "ows:Abstract", abs);
    }
    write_text_element(writer, "ows:ServiceType", "OGC WMTS");
    write_text_element(writer, "ows:ServiceTypeVersion", "1.0.0");
    write_event(
        writer,
        Event::End(BytesEnd::new("ows:ServiceIdentification")),
    );
}

fn write_operations_metadata(writer: &mut Writer<Vec<u8>>, service: &WmtsService) {
    write_event(
        writer,
        Event::Start(BytesStart::new("ows:OperationsMetadata")),
    );
    for op_name in ["GetCapabilities", "GetTile"] {
        let mut op = BytesStart::new("ows:Operation");
        op.push_attribute(("name", op_name));
        write_event(writer, Event::Start(op));
        write_event(writer, Event::Start(BytesStart::new("ows:DCP")));
        write_event(writer, Event::Start(BytesStart::new("ows:HTTP")));

        let mut get = BytesStart::new("ows:Get");
        get.push_attribute(("xlink:href", service.service_url.as_str()));
        write_event(writer, Event::Start(get));

        let mut constraint = BytesStart::new("ows:Constraint");
        constraint.push_attribute(("name", "GetEncoding"));
        write_event(writer, Event::Start(constraint));
        write_event(writer, Event::Start(BytesStart::new("ows:AllowedValues")));
        write_text_element(writer, "ows:Value", "RESTful");
        write_event(writer, Event::End(BytesEnd::new("ows:AllowedValues")));
        write_event(writer, Event::End(BytesEnd::new("ows:Constraint")));

        write_event(writer, Event::End(BytesEnd::new("ows:Get")));
        write_event(writer, Event::End(BytesEnd::new("ows:HTTP")));
        write_event(writer, Event::End(BytesEnd::new("ows:DCP")));
        write_event(writer, Event::End(BytesEnd::new("ows:Operation")));
    }
    write_event(writer, Event::End(BytesEnd::new("ows:OperationsMetadata")));
}

fn write_contents(writer: &mut Writer<Vec<u8>>, layers: &[WmtsLayer]) {
    write_event(writer, Event::Start(BytesStart::new("Contents")));
    for layer in layers {
        write_layer(writer, layer);
    }
    write_tile_matrix_set(writer);
    write_event(writer, Event::End(BytesEnd::new("Contents")));
}

fn write_layer(writer: &mut Writer<Vec<u8>>, layer: &WmtsLayer) {
    write_event(writer, Event::Start(BytesStart::new("Layer")));
    write_text_element(writer, "ows:Title", &layer.title);
    if let Some(abs) = &layer.abstract_ {
        write_text_element(writer, "ows:Abstract", abs);
    }
    write_text_element(writer, "ows:Identifier", &layer.identifier);

    if let Some([w, s, e, n]) = layer.bounding_box {
        let mut bbox = BytesStart::new("ows:WGS84BoundingBox");
        bbox.push_attribute(("crs", "urn:ogc:def:crs:OGC:2:84"));
        write_event(writer, Event::Start(bbox));
        write_text_element(writer, "ows:LowerCorner", &format!("{w} {s}"));
        write_text_element(writer, "ows:UpperCorner", &format!("{e} {n}"));
        write_event(writer, Event::End(BytesEnd::new("ows:WGS84BoundingBox")));
    }

    let mut style = BytesStart::new("Style");
    style.push_attribute(("isDefault", "true"));
    write_event(writer, Event::Start(style));
    write_text_element(writer, "ows:Identifier", "default");
    write_event(writer, Event::End(BytesEnd::new("Style")));

    write_text_element(writer, "Format", &layer.format);

    write_event(writer, Event::Start(BytesStart::new("TileMatrixSetLink")));
    write_text_element(writer, "TileMatrixSet", "GoogleMapsCompatible");
    write_tile_matrix_set_limits(writer, layer.min_zoom, layer.max_zoom);
    write_event(writer, Event::End(BytesEnd::new("TileMatrixSetLink")));

    let mut resource = BytesStart::new("ResourceURL");
    resource.push_attribute(("format", layer.format.as_str()));
    resource.push_attribute(("resourceType", "tile"));
    resource.push_attribute(("template", layer.tile_url_template.as_str()));
    write_event(writer, Event::Empty(resource));

    write_event(writer, Event::End(BytesEnd::new("Layer")));
}

/// Emit `<TileMatrixSetLimits>` covering `min_zoom..=max_zoom` (clamped to the
/// GoogleMapsCompatible 0..=22 ceiling). Each level uses the full matrix extent
/// (`0..=2^z - 1`); spatial bbox-based row/col tightening is intentionally not
/// done here (see design doc §4.1).
fn write_tile_matrix_set_limits(writer: &mut Writer<Vec<u8>>, min_zoom: u8, max_zoom: u8) {
    let hi = max_zoom.min(22);
    write_event(writer, Event::Start(BytesStart::new("TileMatrixSetLimits")));
    for z in min_zoom..=hi {
        let max_idx = (1u32 << z) - 1;
        write_event(writer, Event::Start(BytesStart::new("TileMatrixLimits")));
        write_text_element(writer, "TileMatrix", &z.to_string());
        write_text_element(writer, "MinTileRow", "0");
        write_text_element(writer, "MaxTileRow", &max_idx.to_string());
        write_text_element(writer, "MinTileCol", "0");
        write_text_element(writer, "MaxTileCol", &max_idx.to_string());
        write_event(writer, Event::End(BytesEnd::new("TileMatrixLimits")));
    }
    write_event(writer, Event::End(BytesEnd::new("TileMatrixSetLimits")));
}

/// `GoogleMapsCompatible` TileMatrixSet (zooms 0..=22).
///
/// Numeric scale denominators / corner coordinates are kept as the
/// canonical literals from the WMTS profile.
fn write_tile_matrix_set(writer: &mut Writer<Vec<u8>>) {
    const TOP_LEFT: &str = "-20037508.3427892 20037508.3427892";
    /// `(zoom, scale_denominator, matrix_dim)`. Matrix is `dim × dim`.
    const LEVELS: &[(u8, &str, u32)] = &[
        (0, "559082264.0287178", 1),
        (1, "279541132.0143589", 2),
        (2, "139770566.0071794", 4),
        (3, "69885283.0035897", 8),
        (4, "34942641.5017948", 16),
        (5, "17471320.7508974", 32),
        (6, "8735660.3754487", 64),
        (7, "4367830.1877244", 128),
        (8, "2183915.0938622", 256),
        (9, "1091957.5469311", 512),
        (10, "545978.7734655", 1024),
        (11, "272989.3867328", 2048),
        (12, "136494.6933664", 4096),
        (13, "68247.3466832", 8192),
        (14, "34123.6733416", 16384),
        (15, "17061.8366708", 32768),
        (16, "8530.9183354", 65536),
        (17, "4265.4591677", 131072),
        (18, "2132.7295838", 262144),
        (19, "1066.3647919", 524288),
        (20, "533.1823960", 1048576),
        (21, "266.5911980", 2097152),
        (22, "133.2955990", 4194304),
    ];

    write_event(writer, Event::Start(BytesStart::new("TileMatrixSet")));
    write_text_element(writer, "ows:Title", "GoogleMapsCompatible");
    write_text_element(writer, "ows:Identifier", "GoogleMapsCompatible");
    write_text_element(writer, "ows:SupportedCRS", "urn:ogc:def:crs:EPSG::3857");
    write_text_element(
        writer,
        "WellKnownScaleSet",
        "urn:ogc:def:wkss:OGC:1.0:GoogleMapsCompatible",
    );
    for (zoom, scale, dim) in LEVELS {
        write_event(writer, Event::Start(BytesStart::new("TileMatrix")));
        write_text_element(writer, "ows:Identifier", &zoom.to_string());
        write_text_element(writer, "ScaleDenominator", scale);
        write_text_element(writer, "TopLeftCorner", TOP_LEFT);
        write_text_element(writer, "TileWidth", "256");
        write_text_element(writer, "TileHeight", "256");
        write_text_element(writer, "MatrixWidth", &dim.to_string());
        write_text_element(writer, "MatrixHeight", &dim.to_string());
        write_event(writer, Event::End(BytesEnd::new("TileMatrix")));
    }
    write_event(writer, Event::End(BytesEnd::new("TileMatrixSet")));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_layer() -> WmtsLayer {
        WmtsLayer {
            identifier: "bright".into(),
            title: "OSM Bright".into(),
            abstract_: Some("Light style".into()),
            tile_url_template:
                "https://example.com/styles/bright/{TileMatrix}/{TileCol}/{TileRow}.png".into(),
            format: "image/png".into(),
            bounding_box: Some([-180.0, -85.0, 180.0, 85.0]),
            min_zoom: 0,
            max_zoom: 14,
        }
    }

    fn sample_service() -> WmtsService {
        WmtsService {
            title: "Maptile WMTS".into(),
            abstract_: Some("Self-hosted tile server".into()),
            service_url: "https://example.com/styles/bright/wmts.xml".into(),
        }
    }

    #[test]
    fn xml_starts_with_declaration_and_contains_capabilities() {
        let xml = build_capabilities(&sample_service(), &[sample_layer()]);
        assert!(xml.starts_with("<?xml"));
        assert!(xml.contains("<Capabilities"));
        assert!(xml.contains("</Capabilities>"));
    }

    #[test]
    fn includes_layer_identifier_and_title() {
        let xml = build_capabilities(&sample_service(), &[sample_layer()]);
        assert!(xml.contains("<ows:Identifier>bright</ows:Identifier>"));
        assert!(xml.contains("<ows:Title>OSM Bright</ows:Title>"));
        assert!(xml.contains("<Format>image/png</Format>"));
    }

    #[test]
    fn includes_resource_url_template() {
        let xml = build_capabilities(&sample_service(), &[sample_layer()]);
        assert!(xml.contains(
            "template=\"https://example.com/styles/bright/{TileMatrix}/{TileCol}/{TileRow}.png\""
        ));
    }

    #[test]
    fn includes_tilematrixset_googlemapscompatible_with_zoom_22() {
        let xml = build_capabilities(&sample_service(), &[sample_layer()]);
        assert!(xml.contains("GoogleMapsCompatible"));
        assert!(xml.contains("<ows:Identifier>22</ows:Identifier>"));
    }

    #[test]
    fn xml_escapes_special_chars_in_title() {
        let mut layer = sample_layer();
        layer.title = "A & B <c>".into();
        let xml = build_capabilities(&sample_service(), &[layer]);
        assert!(xml.contains("A &amp; B &lt;c&gt;"));
        assert!(!xml.contains("A & B <c>"));
    }

    #[test]
    fn empty_layer_list_still_produces_valid_xml() {
        let xml = build_capabilities(&sample_service(), &[]);
        assert!(xml.contains("<Contents>"));
        assert!(xml.contains("</Contents>"));
        // No <Layer> blocks.
        assert!(!xml.contains("<Layer>"));
    }

    #[test]
    fn emits_tile_matrix_set_limits_for_layer_zoom_range() {
        // sample_layer() sets min_zoom: 0, max_zoom: 14.
        let xml = build_capabilities(&sample_service(), &[sample_layer()]);
        // Limits block is present on the layer link.
        assert!(xml.contains("<TileMatrixSetLimits>"));
        assert!(xml.contains("<TileMatrixLimits>"));
        // `MaxTileCol` only appears inside our limits block (the global
        // TileMatrixSet emits MatrixWidth/Height, not MaxTileCol), so its values
        // unambiguously pin the advertised zoom range.
        // Level 14 has a 16384x16384 matrix → MaxTileCol = 16383 (limit emitted).
        assert!(xml.contains("<MaxTileCol>16383</MaxTileCol>"));
        // Level 22 (2^22 - 1 = 4194303) must NOT be limited — range stops at 14.
        assert!(!xml.contains("<MaxTileCol>4194303</MaxTileCol>"));
        // Lowest level uses the full extent origin.
        assert!(xml.contains("<MinTileRow>0</MinTileRow>"));
    }
}
