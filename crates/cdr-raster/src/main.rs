use cdr_domain::{ClosedPath, PathSegment as DomainPathSegment, PointMm as DomainPointMm};
use cdr_raster::{
    AlphaMask, HolePlacementSettings, OutlineSettings, PixelBounds, combine_top_center_hole,
};
use geo::{Area, BoundingRect, LineString, MultiPolygon};
use image::{ImageReader, Rgba, RgbaImage, imageops};
use imageproc::drawing::draw_antialiased_line_segment_mut;
use imageproc::pixelops::interpolate;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

const OUTLINE_OFFSET_MM: f64 = 2.0;
const HOLE_DIAMETER_MM: f64 = 3.5;
const HOLE_EDGE_CLEARANCE_MM: f64 = 2.0;
const TOOL_DIAMETER_MM: f64 = 2.0;
const SMOOTHING_MM: f64 = 0.02;
const ALPHA_THRESHOLD: u8 = 127;
const MINIMUM_COMPONENT_AREA_MM2: f64 = 0.25;

#[derive(Debug, Deserialize)]
struct Inventory {
    source_file: String,
    source_sha256: String,
    cdr_saved: bool,
    bitmaps: Vec<BitmapInventory>,
}

#[derive(Debug, Deserialize)]
struct BitmapInventory {
    shape_path: String,
    png: PathBuf,
    pixel_width: u32,
    pixel_height: u32,
    bbox_mm: BoundingBoxMm,
}

#[derive(Debug, Deserialize)]
struct BoundingBoxMm {
    left: f64,
    bottom: f64,
    width: f64,
    height: f64,
}

#[derive(Debug, Serialize)]
struct VectorOutput {
    schema_version: u32,
    source_file: String,
    source_sha256: String,
    page_index: u32,
    shapes: Vec<VectorShape>,
}

#[derive(Debug, Serialize)]
struct VectorShape {
    source_shape_path: String,
    name: &'static str,
    outline_hex: &'static str,
    outline_width_mm: f64,
    polygons: Vec<VectorPolygon>,
}

#[derive(Debug, Serialize)]
struct VectorPolygon {
    exterior: Vec<PointMm>,
    interiors: Vec<Vec<PointMm>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    curved_exterior: Option<ClosedPath>,
    #[serde(skip_serializing_if = "Option::is_none")]
    curved_interiors: Option<Vec<ClosedPath>>,
}

#[derive(Debug, Serialize)]
struct RasterReport {
    schema_version: u32,
    source_file: String,
    source_sha256: String,
    cdr_saved: bool,
    configured_outline_offset_mm: f64,
    configured_hole_diameter_mm: f64,
    configured_hole_edge_clearance_mm: f64,
    configured_tool_diameter_mm: f64,
    configured_smoothing_mm: f64,
    alpha_threshold: u8,
    minimum_component_area_mm2: f64,
    bitmaps: Vec<BitmapReport>,
}

#[derive(Debug, Serialize)]
struct BitmapReport {
    shape_path: String,
    png: PathBuf,
    pixel_width: u32,
    pixel_height: u32,
    pixel_width_mm: f64,
    pixel_height_mm: f64,
    transparent_pixels: u64,
    partial_pixels: u64,
    opaque_pixels: u64,
    content_bounds_px: Option<SerializablePixelBounds>,
    alpha_edge_available: bool,
    status: &'static str,
    preview_svg: Option<PathBuf>,
    preview_png: Option<PathBuf>,
    outline_components: Option<usize>,
    hole_cutouts: Option<usize>,
    base_outline_area_mm2: Option<f64>,
    outline_area_mm2: Option<f64>,
    hole_center_mm: Option<PointMm>,
}

#[derive(Debug, Serialize)]
struct SerializablePixelBounds {
    left: u32,
    top: u32,
    right: u32,
    bottom: u32,
}

#[derive(Debug, Clone, Serialize)]
struct PointMm {
    x: f64,
    y: f64,
}

impl From<PixelBounds> for SerializablePixelBounds {
    fn from(value: PixelBounds) -> Self {
        Self {
            left: value.left,
            top: value.top,
            right: value.right,
            bottom: value.bottom,
        }
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let inventory_path = PathBuf::from(
        arguments
            .next()
            .ok_or("usage: cdr-raster <inventory.json> <raster-report.json>")?,
    );
    let report_path = PathBuf::from(
        arguments
            .next()
            .ok_or("usage: cdr-raster <inventory.json> <raster-report.json>")?,
    );
    if arguments.next().is_some() {
        return Err("too many arguments".into());
    }

    let inventory: Inventory = serde_json::from_slice(&fs::read(&inventory_path)?)?;
    let base_directory = inventory_path.parent().unwrap_or_else(|| Path::new("."));
    let report_directory = report_path.parent().unwrap_or_else(|| Path::new("."));
    let previews_directory = report_directory.join("previews");
    fs::create_dir_all(&previews_directory)?;
    let mut bitmaps = Vec::with_capacity(inventory.bitmaps.len());
    let mut vector_shapes = Vec::new();

    for entry in inventory.bitmaps {
        let image_path = base_directory.join(&entry.png);
        let image = ImageReader::open(&image_path)?.decode()?.into_rgba8();
        if image.width() != entry.pixel_width || image.height() != entry.pixel_height {
            return Err(format!(
                "{} dimensions differ from the CDR inventory",
                entry.shape_path
            )
            .into());
        }

        let alpha = image.pixels().map(|pixel| pixel.0[3]).collect();
        let mask = AlphaMask::try_new(image.width(), image.height(), alpha)?;
        let stats = mask.stats();
        let alpha_edge_available = stats.has_transparent_edge();
        let (
            preview_svg,
            preview_png,
            outline_components,
            hole_cutouts,
            base_outline_area_mm2,
            outline_area_mm2,
            hole_center_mm,
        ) = if alpha_edge_available {
            let base_outline = mask.offset_outline(
                entry.bbox_mm.width,
                entry.bbox_mm.height,
                OutlineSettings {
                    alpha_threshold: ALPHA_THRESHOLD,
                    offset_mm: OUTLINE_OFFSET_MM,
                    tool_diameter_mm: TOOL_DIAMETER_MM,
                    smoothing_mm: SMOOTHING_MM,
                    minimum_component_area_mm2: MINIMUM_COMPONENT_AREA_MM2,
                },
            )?;
            let combined = combine_top_center_hole(
                &base_outline,
                HolePlacementSettings {
                    diameter_mm: HOLE_DIAMETER_MM,
                    edge_clearance_mm: HOLE_EDGE_CLEARANCE_MM,
                    smoothing_mm: SMOOTHING_MM,
                },
            )?;
            let preview_name = format!("outline-{}.svg", entry.shape_path.replace('.', "-"));
            let preview_path = previews_directory.join(&preview_name);
            let image_href = Path::new("..").join(&entry.png);
            write_outline_svg(
                &preview_path,
                &image_href,
                entry.bbox_mm.width,
                entry.bbox_mm.height,
                &combined.geometry,
                combined.curved_geometry.as_deref(),
            )?;
            let preview_png_name = format!("outline-{}.png", entry.shape_path.replace('.', "-"));
            let preview_png_path = previews_directory.join(&preview_png_name);
            write_outline_png(
                &preview_png_path,
                &image,
                entry.bbox_mm.width,
                entry.bbox_mm.height,
                &combined.geometry,
            )?;
            vector_shapes.push(VectorShape {
                source_shape_path: entry.shape_path.clone(),
                name: "h_xbxb",
                outline_hex: "#EC2A90",
                outline_width_mm: 0.076,
                polygons: to_page_polygons(
                    &combined.geometry,
                    &entry.bbox_mm,
                    combined.curved_geometry.as_deref(),
                ),
            });
            (
                Some(Path::new("previews").join(preview_name)),
                Some(Path::new("previews").join(preview_png_name)),
                Some(combined.geometry.0.len()),
                Some(
                    combined
                        .geometry
                        .iter()
                        .map(|polygon| polygon.interiors().len())
                        .sum(),
                ),
                Some(base_outline.unsigned_area()),
                Some(combined.geometry.unsigned_area()),
                combined.hole_center.map(|center| PointMm {
                    x: center.x(),
                    y: center.y(),
                }),
            )
        } else {
            (None, None, None, None, None, None, None)
        };
        bitmaps.push(BitmapReport {
            shape_path: entry.shape_path,
            png: entry.png,
            pixel_width: mask.width(),
            pixel_height: mask.height(),
            pixel_width_mm: entry.bbox_mm.width / f64::from(mask.width()),
            pixel_height_mm: entry.bbox_mm.height / f64::from(mask.height()),
            transparent_pixels: stats.transparent_pixels,
            partial_pixels: stats.partial_pixels,
            opaque_pixels: stats.opaque_pixels,
            content_bounds_px: stats.content_bounds.map(Into::into),
            alpha_edge_available,
            status: if alpha_edge_available {
                "combined_preview_generated"
            } else {
                "no_transparent_pixels"
            },
            preview_svg,
            preview_png,
            outline_components,
            hole_cutouts,
            base_outline_area_mm2,
            outline_area_mm2,
            hole_center_mm,
        });
    }

    let vector_output = VectorOutput {
        schema_version: 1,
        source_file: inventory.source_file.clone(),
        source_sha256: inventory.source_sha256.clone(),
        page_index: 1,
        shapes: vector_shapes,
    };
    fs::write(
        report_directory.join("vector-output.json"),
        serde_json::to_vec_pretty(&vector_output)?,
    )?;

    let report = RasterReport {
        schema_version: 1,
        source_file: inventory.source_file,
        source_sha256: inventory.source_sha256,
        cdr_saved: inventory.cdr_saved,
        configured_outline_offset_mm: OUTLINE_OFFSET_MM,
        configured_hole_diameter_mm: HOLE_DIAMETER_MM,
        configured_hole_edge_clearance_mm: HOLE_EDGE_CLEARANCE_MM,
        configured_tool_diameter_mm: TOOL_DIAMETER_MM,
        configured_smoothing_mm: SMOOTHING_MM,
        alpha_threshold: ALPHA_THRESHOLD,
        minimum_component_area_mm2: MINIMUM_COMPONENT_AREA_MM2,
        bitmaps,
    };
    fs::write(report_path, serde_json::to_vec_pretty(&report)?)?;
    Ok(())
}

fn to_page_polygons(
    geometry: &MultiPolygon<f64>,
    bounds: &BoundingBoxMm,
    curved_geometry: Option<&[cdr_raster::CurvedPolygon]>,
) -> Vec<VectorPolygon> {
    geometry
        .iter()
        .enumerate()
        .map(|(index, polygon)| {
            let curved = curved_geometry.and_then(|curves| curves.get(index));
            VectorPolygon {
                exterior: to_page_ring(polygon.exterior(), bounds, true),
                interiors: polygon
                    .interiors()
                    .iter()
                    .map(|ring| to_page_ring(ring, bounds, false))
                    .collect(),
                curved_exterior: curved
                    .map(|polygon| to_page_curve_path(&polygon.exterior, bounds, true)),
                curved_interiors: curved.map(|polygon| {
                    polygon
                        .interiors
                        .iter()
                        .map(|path| to_page_curve_path(path, bounds, false))
                        .collect()
                }),
            }
        })
        .collect()
}

fn to_page_curve_path(
    path: &ClosedPath,
    bounds: &BoundingBoxMm,
    counter_clockwise: bool,
) -> ClosedPath {
    let transform = |point: DomainPointMm| DomainPointMm {
        x: bounds.left + point.x,
        y: bounds.bottom + bounds.height - point.y,
    };
    let segments = path
        .segments()
        .iter()
        .map(|segment| match segment {
            DomainPathSegment::Line { start, end } => DomainPathSegment::Line {
                start: transform(*start),
                end: transform(*end),
            },
            DomainPathSegment::CubicBezier {
                start,
                control_start,
                control_end,
                end,
            } => DomainPathSegment::CubicBezier {
                start: transform(*start),
                control_start: transform(*control_start),
                control_end: transform(*control_end),
                end: transform(*end),
            },
        })
        .collect();
    let transformed = ClosedPath::try_new(segments).expect("transformed path remains closed");
    let anchors = transformed
        .segments()
        .iter()
        .map(|segment| {
            let point = segment.start();
            PointMm {
                x: point.x,
                y: point.y,
            }
        })
        .collect::<Vec<_>>();
    if (signed_ring_area(&anchors) > 0.0) == counter_clockwise {
        transformed
    } else {
        let reversed = transformed
            .segments()
            .iter()
            .rev()
            .map(|segment| match segment {
                DomainPathSegment::Line { start, end } => DomainPathSegment::Line {
                    start: *end,
                    end: *start,
                },
                DomainPathSegment::CubicBezier {
                    start,
                    control_start,
                    control_end,
                    end,
                } => DomainPathSegment::CubicBezier {
                    start: *end,
                    control_start: *control_end,
                    control_end: *control_start,
                    end: *start,
                },
            })
            .collect();
        ClosedPath::try_new(reversed).expect("reversed path remains closed")
    }
}

fn to_page_ring(
    ring: &LineString<f64>,
    bounds: &BoundingBoxMm,
    counter_clockwise: bool,
) -> Vec<PointMm> {
    let mut points = ring
        .coords()
        .map(|coordinate| PointMm {
            x: bounds.left + coordinate.x,
            y: bounds.bottom + bounds.height - coordinate.y,
        })
        .collect::<Vec<_>>();
    if points.len() > 1 {
        let last = points.len() - 1;
        if (points[0].x - points[last].x).abs() < 1.0e-9
            && (points[0].y - points[last].y).abs() < 1.0e-9
        {
            points.pop();
        }
    }
    let is_counter_clockwise = signed_ring_area(&points) > 0.0;
    if is_counter_clockwise != counter_clockwise {
        points.reverse();
    }
    points
}

fn signed_ring_area(points: &[PointMm]) -> f64 {
    if points.len() < 3 {
        return 0.0;
    }
    points
        .iter()
        .zip(points.iter().cycle().skip(1))
        .take(points.len())
        .map(|(first, second)| first.x * second.y - second.x * first.y)
        .sum::<f64>()
        / 2.0
}

fn write_outline_png(
    path: &Path,
    image: &RgbaImage,
    image_width_mm: f64,
    image_height_mm: f64,
    outline: &MultiPolygon<f64>,
) -> Result<(), Box<dyn Error>> {
    let bounds = outline
        .bounding_rect()
        .ok_or("offset outline does not have a bounding rectangle")?;
    let margin = 1.0;
    let min_x = bounds.min().x - margin;
    let min_y = bounds.min().y - margin;
    let width_mm = bounds.width() + margin * 2.0;
    let height_mm = bounds.height() + margin * 2.0;
    let source_scale = f64::from(image.width()) / image_width_mm;
    let scale = source_scale.clamp(8.0, 30.0);
    let canvas_width = (width_mm * scale).ceil() as u32;
    let canvas_height = (height_mm * scale).ceil() as u32;
    let mut canvas = RgbaImage::from_fn(canvas_width, canvas_height, |x, y| {
        let checker = ((x / 16) + (y / 16)) % 2 == 0;
        if checker {
            Rgba([255, 255, 255, 255])
        } else {
            Rgba([238, 238, 238, 255])
        }
    });
    let rendered_width = (image_width_mm * scale).round() as u32;
    let rendered_height = (image_height_mm * scale).round() as u32;
    let rendered = imageops::resize(
        image,
        rendered_width,
        rendered_height,
        imageops::FilterType::Lanczos3,
    );
    let image_x = ((0.0 - min_x) * scale).round() as i64;
    let image_y = ((0.0 - min_y) * scale).round() as i64;
    imageops::overlay(&mut canvas, &rendered, image_x, image_y);

    for polygon in outline {
        draw_png_ring(&mut canvas, polygon.exterior(), min_x, min_y, scale);
        for interior in polygon.interiors() {
            draw_png_ring(&mut canvas, interior, min_x, min_y, scale);
        }
    }
    canvas.save(path)?;
    Ok(())
}

fn draw_png_ring(
    canvas: &mut RgbaImage,
    ring: &LineString<f64>,
    min_x: f64,
    min_y: f64,
    scale: f64,
) {
    let to_pixel = |x: f64, y: f64| {
        (
            ((x - min_x) * scale).round() as i32,
            ((y - min_y) * scale).round() as i32,
        )
    };
    for segment in ring.lines() {
        draw_antialiased_line_segment_mut(
            canvas,
            to_pixel(segment.start.x, segment.start.y),
            to_pixel(segment.end.x, segment.end.y),
            Rgba([236, 42, 144, 255]),
            interpolate,
        );
    }
}

fn write_outline_svg(
    path: &Path,
    image_href: &Path,
    image_width_mm: f64,
    image_height_mm: f64,
    outline: &MultiPolygon<f64>,
    curved_geometry: Option<&[cdr_raster::CurvedPolygon]>,
) -> Result<(), Box<dyn Error>> {
    let bounds = outline
        .bounding_rect()
        .ok_or("offset outline does not have a bounding rectangle")?;
    let margin = 1.0;
    let min_x = bounds.min().x - margin;
    let min_y = bounds.min().y - margin;
    let width = bounds.width() + margin * 2.0;
    let height = bounds.height() + margin * 2.0;
    let mut path_data = String::new();
    for (index, polygon) in outline.iter().enumerate() {
        if let Some(curved) = curved_geometry.and_then(|curves| curves.get(index)) {
            append_svg_curve_path(&mut path_data, &curved.exterior);
            for interior in &curved.interiors {
                append_svg_curve_path(&mut path_data, interior);
            }
        } else {
            append_svg_ring(&mut path_data, polygon.exterior());
            for interior in polygon.interiors() {
                append_svg_ring(&mut path_data, interior);
            }
        }
    }

    let image_href = escape_xml(&image_href.to_string_lossy().replace('\\', "/"));
    let mut svg = String::new();
    writeln!(
        svg,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{width:.4}mm" height="{height:.4}mm" viewBox="{min_x:.4} {min_y:.4} {width:.4} {height:.4}">"#
    )?;
    writeln!(
        svg,
        r##"  <defs><pattern id="checker" width="4" height="4" patternUnits="userSpaceOnUse"><rect width="4" height="4" fill="#ffffff"/><path d="M0 0h2v2H0zM2 2h2v2H2z" fill="#eeeeee"/></pattern></defs>"##
    )?;
    writeln!(
        svg,
        r#"  <rect x="{min_x:.4}" y="{min_y:.4}" width="{width:.4}" height="{height:.4}" fill="url(#checker)"/>"#
    )?;
    writeln!(
        svg,
        r#"  <image href="{image_href}" x="0" y="0" width="{image_width_mm:.4}" height="{image_height_mm:.4}" preserveAspectRatio="none"/>"#
    )?;
    writeln!(
        svg,
        r##"  <path d="{path_data}" fill="none" fill-rule="evenodd" stroke="#EC2A90" stroke-width="0.076" stroke-linejoin="round" stroke-linecap="round"/>"##
    )?;
    svg.push_str("</svg>\n");
    fs::write(path, svg)?;
    Ok(())
}

fn append_svg_ring(path: &mut String, ring: &LineString<f64>) {
    let mut coordinates = ring.coords();
    let Some(first) = coordinates.next() else {
        return;
    };
    let _ = write!(path, "M{:.4} {:.4}", first.x, first.y);
    for coordinate in coordinates {
        let _ = write!(path, "L{:.4} {:.4}", coordinate.x, coordinate.y);
    }
    path.push('Z');
}

fn append_svg_curve_path(path: &mut String, ring: &ClosedPath) {
    let Some(first_segment) = ring.segments().first() else {
        return;
    };
    let first = first_segment.start();
    let _ = write!(path, "M{:.4} {:.4}", first.x, first.y);
    for segment in ring.segments() {
        match segment {
            DomainPathSegment::Line { end, .. } => {
                let _ = write!(path, "L{:.4} {:.4}", end.x, end.y);
            }
            DomainPathSegment::CubicBezier {
                control_start,
                control_end,
                end,
                ..
            } => {
                let _ = write!(
                    path,
                    "C{:.4} {:.4} {:.4} {:.4} {:.4} {:.4}",
                    control_start.x, control_start.y, control_end.x, control_end.y, end.x, end.y
                );
            }
        }
    }
    path.push('Z');
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_image_coordinates_to_page_coordinates() {
        let ring = LineString::from(vec![
            (0.0, 0.0),
            (10.0, 0.0),
            (10.0, 20.0),
            (0.0, 20.0),
            (0.0, 0.0),
        ]);
        let bounds = BoundingBoxMm {
            left: 30.0,
            bottom: 40.0,
            width: 10.0,
            height: 20.0,
        };

        let points = to_page_ring(&ring, &bounds, true);

        assert_eq!(points.len(), 4);
        for expected in [(30.0, 40.0), (30.0, 60.0), (40.0, 40.0), (40.0, 60.0)] {
            assert!(points.iter().any(|point| (point.x, point.y) == expected));
        }
        assert!(signed_ring_area(&points) > 0.0);
    }

    #[test]
    fn gives_holes_the_opposite_winding_direction() {
        let ring = LineString::from(vec![
            (0.0, 0.0),
            (4.0, 0.0),
            (4.0, 4.0),
            (0.0, 4.0),
            (0.0, 0.0),
        ]);
        let bounds = BoundingBoxMm {
            left: 0.0,
            bottom: 0.0,
            width: 4.0,
            height: 4.0,
        };

        let points = to_page_ring(&ring, &bounds, false);

        assert!(signed_ring_area(&points) < 0.0);
    }
}
