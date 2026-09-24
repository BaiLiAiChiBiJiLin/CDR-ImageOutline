#![forbid(unsafe_code)]

use std::error::Error;
use std::fmt;

use cdr_domain::{ClosedPath, PathSegment, PointMm};
use contour::ContourBuilder;
use geo::{
    Area, BooleanOps, BoundingRect, Buffer, Coord, LineString, MultiPolygon, Point, Polygon,
    Simplify,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelBounds {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlphaStats {
    pub transparent_pixels: u64,
    pub partial_pixels: u64,
    pub opaque_pixels: u64,
    pub content_bounds: Option<PixelBounds>,
}

impl AlphaStats {
    pub const fn has_transparent_edge(self) -> bool {
        self.transparent_pixels > 0 && (self.partial_pixels > 0 || self.opaque_pixels > 0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlphaMask {
    width: u32,
    height: u32,
    values: Vec<u8>,
}

impl AlphaMask {
    pub fn try_new(width: u32, height: u32, values: Vec<u8>) -> Result<Self, RasterError> {
        let expected_len = usize::try_from(u64::from(width) * u64::from(height))
            .map_err(|_| RasterError::DimensionsTooLarge)?;
        if values.len() != expected_len {
            return Err(RasterError::AlphaLengthMismatch {
                expected: expected_len,
                actual: values.len(),
            });
        }
        Ok(Self {
            width,
            height,
            values,
        })
    }

    pub const fn width(&self) -> u32 {
        self.width
    }

    pub const fn height(&self) -> u32 {
        self.height
    }

    pub fn stats(&self) -> AlphaStats {
        let mut transparent_pixels = 0;
        let mut partial_pixels = 0;
        let mut opaque_pixels = 0;
        let mut bounds: Option<PixelBounds> = None;

        for (index, alpha) in self.values.iter().copied().enumerate() {
            match alpha {
                0 => transparent_pixels += 1,
                255 => opaque_pixels += 1,
                _ => partial_pixels += 1,
            }

            if alpha == 0 {
                continue;
            }

            let width = self.width as usize;
            let x = u32::try_from(index % width).expect("x is less than mask width");
            let y = u32::try_from(index / width).expect("y is less than mask height");
            bounds = Some(match bounds {
                None => PixelBounds {
                    left: x,
                    top: y,
                    right: x,
                    bottom: y,
                },
                Some(current) => PixelBounds {
                    left: current.left.min(x),
                    top: current.top.min(y),
                    right: current.right.max(x),
                    bottom: current.bottom.max(y),
                },
            });
        }

        AlphaStats {
            transparent_pixels,
            partial_pixels,
            opaque_pixels,
            content_bounds: bounds,
        }
    }

    /// Returns the inclusive bounds of pixels whose alpha is above the supplied threshold.
    pub fn content_bounds_above(&self, alpha_threshold: u8) -> Option<PixelBounds> {
        let mut bounds: Option<PixelBounds> = None;
        let width = self.width as usize;
        for (index, alpha) in self.values.iter().copied().enumerate() {
            if alpha <= alpha_threshold {
                continue;
            }
            let x = u32::try_from(index % width).expect("x is less than mask width");
            let y = u32::try_from(index / width).expect("y is less than mask height");
            bounds = Some(match bounds {
                None => PixelBounds {
                    left: x,
                    top: y,
                    right: x,
                    bottom: y,
                },
                Some(current) => PixelBounds {
                    left: current.left.min(x),
                    top: current.top.min(y),
                    right: current.right.max(x),
                    bottom: current.bottom.max(y),
                },
            });
        }
        bounds
    }

    /// Creates a binary alpha silhouette and softens its boundary by one pixel.
    pub fn antialiased_threshold(&self, alpha_threshold: u8) -> Result<Vec<u8>, RasterError> {
        let binary = self
            .values
            .iter()
            .map(|alpha| if *alpha > alpha_threshold { 255 } else { 0 })
            .collect::<Vec<_>>();
        smooth_binary_alpha(&binary, self.width, self.height)
    }

    pub fn offset_outline(
        &self,
        physical_width_mm: f64,
        physical_height_mm: f64,
        settings: OutlineSettings,
    ) -> Result<MultiPolygon<f64>, RasterError> {
        if !settings.offset_mm.is_finite() || settings.offset_mm <= 0.0 {
            return Err(RasterError::InvalidOutlineSettings);
        }
        let contour = self.trace_contour(physical_width_mm, physical_height_mm, settings)?;
        let buffered = contour.buffer(settings.offset_mm);
        let exterior_only = buffered
            .0
            .into_iter()
            .map(|polygon| Polygon::new(polygon.exterior().clone(), Vec::new()))
            .filter(|polygon| polygon.unsigned_area() >= settings.minimum_component_area_mm2)
            .collect::<Vec<_>>();
        if exterior_only.is_empty() {
            return Err(RasterError::NoUsableContour);
        }
        let outline = MultiPolygon::new(exterior_only).simplify(settings.smoothing_mm);
        let tool_closed = close_narrow_grooves(
            &outline,
            settings.tool_diameter_mm,
            settings.minimum_component_area_mm2,
        );
        if tool_closed.0.is_empty() {
            return Err(RasterError::NoUsableContour);
        }
        Ok(tool_closed.simplify(settings.smoothing_mm))
    }

    /// Traces the visible alpha contour without adding an outer expansion.
    /// This is used for the transparent-edge cleanup operation so its vector
    /// edge stays on the bitmap boundary instead of becoming a normal 2 mm
    ///巡边.
    pub fn trace_contour(
        &self,
        physical_width_mm: f64,
        physical_height_mm: f64,
        settings: OutlineSettings,
    ) -> Result<MultiPolygon<f64>, RasterError> {
        if !physical_width_mm.is_finite()
            || !physical_height_mm.is_finite()
            || physical_width_mm <= 0.0
            || physical_height_mm <= 0.0
        {
            return Err(RasterError::InvalidPhysicalSize);
        }
        if !settings.tool_diameter_mm.is_finite()
            || settings.tool_diameter_mm < 0.0
            || !settings.smoothing_mm.is_finite()
            || settings.smoothing_mm < 0.0
            || !settings.minimum_component_area_mm2.is_finite()
            || settings.minimum_component_area_mm2 < 0.0
        {
            return Err(RasterError::InvalidOutlineSettings);
        }
        if !self.stats().has_transparent_edge() {
            return Err(RasterError::NoTransparentEdge);
        }

        let source_width = self.width as usize;
        let source_height = self.height as usize;
        let padded_width = source_width
            .checked_add(2)
            .ok_or(RasterError::DimensionsTooLarge)?;
        let padded_height = source_height
            .checked_add(2)
            .ok_or(RasterError::DimensionsTooLarge)?;
        let padded_len = padded_width
            .checked_mul(padded_height)
            .ok_or(RasterError::DimensionsTooLarge)?;
        let mut values = vec![0.0; padded_len];

        let thresholded_alpha = threshold_alpha(self, settings.alpha_threshold)?;
        for y in 0..source_height {
            let source_start = y * source_width;
            let target_start = (y + 1) * padded_width + 1;
            for x in 0..source_width {
                values[target_start + x] = f64::from(thresholded_alpha[source_start + x]);
            }
        }

        let pixel_width_mm = physical_width_mm / f64::from(self.width);
        let pixel_height_mm = physical_height_mm / f64::from(self.height);
        let contours = ContourBuilder::new(padded_width, padded_height, true)
            .x_origin(-pixel_width_mm)
            .y_origin(-pixel_height_mm)
            .x_step(pixel_width_mm)
            .y_step(pixel_height_mm)
            .contours(&values, &[f64::from(settings.alpha_threshold) + 0.5])
            .map_err(|error| RasterError::Contour(error.to_string()))?;
        let geometry = contours
            .into_iter()
            .next()
            .ok_or(RasterError::NoUsableContour)?
            .into_inner()
            .0;
        let components = geometry
            .0
            .into_iter()
            .map(|polygon| Polygon::new(polygon.exterior().clone(), Vec::new()))
            .filter(|polygon| polygon.unsigned_area() >= settings.minimum_component_area_mm2)
            .collect::<Vec<_>>();
        if components.is_empty() {
            return Err(RasterError::NoUsableContour);
        }

        let simplified = MultiPolygon::new(components).simplify(settings.smoothing_mm);
        let exterior_only = simplified
            .0
            .into_iter()
            .map(|polygon| Polygon::new(polygon.exterior().clone(), Vec::new()))
            .filter(|polygon| polygon.unsigned_area() >= settings.minimum_component_area_mm2)
            .collect::<Vec<_>>();
        if exterior_only.is_empty() {
            return Err(RasterError::NoUsableContour);
        }
        Ok(MultiPolygon::new(exterior_only).simplify(settings.smoothing_mm))
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutlineSettings {
    pub alpha_threshold: u8,
    pub offset_mm: f64,
    /// Minimum tool diameter in millimeters. Narrower inward grooves are closed.
    pub tool_diameter_mm: f64,
    /// Douglas-Peucker tolerance in millimeters. Zero preserves all contour points.
    pub smoothing_mm: f64,
    pub minimum_component_area_mm2: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HolePlacementSettings {
    pub diameter_mm: f64,
    pub edge_clearance_mm: f64,
    pub smoothing_mm: f64,
}

fn close_narrow_grooves(
    outline: &MultiPolygon<f64>,
    tool_diameter_mm: f64,
    minimum_component_area_mm2: f64,
) -> MultiPolygon<f64> {
    if tool_diameter_mm <= 0.0 {
        return outline.clone();
    }

    let radius = tool_diameter_mm / 2.0;
    let dilated = outline.buffer(radius);
    let eroded = dilated.buffer(-radius);
    MultiPolygon::new(
        eroded
            .0
            .into_iter()
            .map(|polygon| Polygon::new(polygon.exterior().clone(), Vec::new()))
            .filter(|polygon| {
                polygon.unsigned_area() > f64::EPSILON
                    && polygon.unsigned_area() >= minimum_component_area_mm2
            })
            .collect(),
    )
}

#[derive(Debug, Clone, PartialEq)]
pub struct CombinedCutPath {
    pub geometry: MultiPolygon<f64>,
    pub hole_center: Option<Point<f64>>,
    pub curved_geometry: Option<Vec<CurvedPolygon>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CurvedPolygon {
    pub exterior: ClosedPath,
    pub interiors: Vec<ClosedPath>,
}

const ROUNDED_SQUARE_CIRCLE_VERTICES: usize = 24;

/// Represents a square whose four corners have radius equal to half its side.
/// That construction is a circle; 24 points approximate its four quarter-arcs
/// with fewer vertices than geo's default round buffer.
fn rounded_square_circle(center: Point<f64>, radius: f64) -> MultiPolygon<f64> {
    let mut coordinates = (0..ROUNDED_SQUARE_CIRCLE_VERTICES)
        .map(|index| {
            let angle =
                std::f64::consts::TAU * index as f64 / ROUNDED_SQUARE_CIRCLE_VERTICES as f64;
            Coord {
                x: center.x() + radius * angle.cos(),
                y: center.y() + radius * angle.sin(),
            }
        })
        .collect::<Vec<_>>();
    coordinates.push(coordinates[0]);
    MultiPolygon::from(Polygon::new(LineString::new(coordinates), Vec::new()))
}

pub fn combine_top_center_hole(
    outline: &MultiPolygon<f64>,
    settings: HolePlacementSettings,
) -> Result<CombinedCutPath, RasterError> {
    validate_hole_settings(settings)?;

    if settings.diameter_mm == 0.0 {
        return Ok(CombinedCutPath {
            geometry: outline.clone(),
            hole_center: None,
            curved_geometry: None,
        });
    }

    let bounds = outline
        .bounding_rect()
        .ok_or(RasterError::NoUsableContour)?;
    let center_x = (bounds.min().x + bounds.max().x) / 2.0;
    let top_y =
        top_boundary_intersection(outline, center_x).ok_or(RasterError::NoTopCenterIntersection)?;
    let hole_center = Point::new(center_x, top_y);
    let hole_radius = settings.diameter_mm / 2.0;
    let support_radius = hole_radius + settings.edge_clearance_mm;
    let support = rounded_square_circle(hole_center, support_radius);
    let hole = rounded_square_circle(hole_center, hole_radius);
    let geometry = outline
        .union(&support)
        .difference(&hole)
        .simplify(settings.smoothing_mm);
    let curved_geometry =
        curved_polygons_from_geometry(&geometry, hole_center, hole_radius, support_radius)?;

    Ok(CombinedCutPath {
        geometry,
        hole_center: Some(hole_center),
        curved_geometry: Some(curved_geometry),
    })
}

/// Creates the not-yet-merged keyhole ring at the top-center of an outline.
pub fn add_top_center_hole_ring(
    outline: &MultiPolygon<f64>,
    settings: HolePlacementSettings,
) -> Result<CombinedCutPath, RasterError> {
    validate_hole_settings(settings)?;
    if settings.diameter_mm == 0.0 {
        return Ok(CombinedCutPath {
            geometry: MultiPolygon::new(Vec::new()),
            hole_center: None,
            curved_geometry: None,
        });
    }

    let bounds = outline
        .bounding_rect()
        .ok_or(RasterError::NoUsableContour)?;
    let center_x = (bounds.min().x + bounds.max().x) / 2.0;
    let top_y =
        top_boundary_intersection(outline, center_x).ok_or(RasterError::NoTopCenterIntersection)?;
    let hole_center = Point::new(center_x, top_y);
    let hole_radius = settings.diameter_mm / 2.0;
    let support_radius = hole_radius + settings.edge_clearance_mm;
    let support = rounded_square_circle(hole_center, support_radius);
    let hole = rounded_square_circle(hole_center, hole_radius);
    let curved_geometry = vec![CurvedPolygon {
        exterior: circle_curve_path(hole_center, support_radius, true),
        interiors: vec![circle_curve_path(hole_center, hole_radius, false)],
    }];

    Ok(CombinedCutPath {
        geometry: support.difference(&hole).simplify(settings.smoothing_mm),
        hole_center: Some(hole_center),
        curved_geometry: Some(curved_geometry),
    })
}

fn curved_polygons_from_geometry(
    geometry: &MultiPolygon<f64>,
    circle_center: Point<f64>,
    hole_radius: f64,
    support_radius: f64,
) -> Result<Vec<CurvedPolygon>, RasterError> {
    geometry
        .iter()
        .map(|polygon| {
            let exterior =
                ring_with_circle_arcs(polygon.exterior(), circle_center, support_radius)?;
            let interiors = polygon
                .interiors()
                .iter()
                .map(|ring| {
                    if ring_is_circle(ring, circle_center, hole_radius) {
                        Ok(circle_curve_path(
                            circle_center,
                            hole_radius,
                            signed_ring_area(ring) > 0.0,
                        ))
                    } else {
                        line_curve_path(ring)
                    }
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(CurvedPolygon {
                exterior,
                interiors,
            })
        })
        .collect()
}

fn ring_with_circle_arcs(
    ring: &LineString<f64>,
    center: Point<f64>,
    radius: f64,
) -> Result<ClosedPath, RasterError> {
    if ring_is_circle(ring, center, radius) {
        return Ok(circle_curve_path(
            center,
            radius,
            signed_ring_area(ring) > 0.0,
        ));
    }

    let mut points = ring.coords().copied().collect::<Vec<_>>();
    if points.len() > 1 && points.first() == points.last() {
        points.pop();
    }
    if points.len() < 3 {
        return Err(RasterError::NoUsableContour);
    }

    let counter_clockwise = signed_ring_area(ring) > 0.0;
    let tolerance = radius * (1.0 - (std::f64::consts::PI / 24.0).cos()) + 1.0e-6;
    let circular_edges = (0..points.len())
        .map(|index| {
            let start = points[index];
            let end = points[(index + 1) % points.len()];
            let midpoint = geo::Coord {
                x: (start.x + end.x) / 2.0,
                y: (start.y + end.y) / 2.0,
            };
            let start_angle = (start.y - center.y()).atan2(start.x - center.x());
            let end_angle = (end.y - center.y()).atan2(end.x - center.x());
            let sweep = angle_sweep(start_angle, end_angle, counter_clockwise);
            (distance_from_circle(start, center, radius) <= tolerance
                && distance_from_circle(end, center, radius) <= tolerance
                && distance_from_circle(midpoint, center, radius) <= tolerance
                && sweep.abs() <= std::f64::consts::PI / 6.0)
                .then_some(sweep)
        })
        .collect::<Vec<_>>();

    if circular_edges.iter().all(Option::is_none) {
        return line_curve_path(ring);
    }

    if circular_edges.iter().all(Option::is_some) {
        return Ok(circle_curve_path(center, radius, counter_clockwise));
    }

    let first_linear_edge = circular_edges
        .iter()
        .position(Option::is_none)
        .ok_or(RasterError::NoUsableContour)?;
    let mut segments = Vec::with_capacity(points.len());
    let mut offset = 0;
    while offset < points.len() {
        let edge_index = (first_linear_edge + 1 + offset) % points.len();
        let start = points[edge_index];
        match circular_edges[edge_index] {
            None => {
                let end = points[(edge_index + 1) % points.len()];
                segments.push(PathSegment::Line {
                    start: point_mm(start),
                    end: point_mm(end),
                });
                offset += 1;
            }
            Some(_) => {
                let mut edge_count = 0;
                let mut sweep = 0.0;
                while offset + edge_count < points.len() {
                    let current = (first_linear_edge + 1 + offset + edge_count) % points.len();
                    let Some(edge_sweep) = circular_edges[current] else {
                        break;
                    };
                    sweep += edge_sweep;
                    edge_count += 1;
                }
                let end = points[(edge_index + edge_count) % points.len()];
                segments.extend(circle_arc_segments(start, end, center, radius, sweep));
                offset += edge_count;
            }
        }
    }

    ClosedPath::try_new(segments).map_err(|_| RasterError::NoUsableContour)
}

fn ring_is_circle(ring: &LineString<f64>, center: Point<f64>, radius: f64) -> bool {
    ring.coords().all(|coordinate| {
        distance_from_circle(*coordinate, center, radius) <= radius * 1.0e-4 + 1.0e-7
    })
}

fn distance_from_circle(coordinate: geo::Coord<f64>, center: Point<f64>, radius: f64) -> f64 {
    ((coordinate.x - center.x()).hypot(coordinate.y - center.y()) - radius).abs()
}

fn angle_sweep(start: f64, end: f64, counter_clockwise: bool) -> f64 {
    if counter_clockwise {
        (end - start).rem_euclid(std::f64::consts::TAU)
    } else {
        -((start - end).rem_euclid(std::f64::consts::TAU))
    }
}

fn circle_arc_segments(
    start: geo::Coord<f64>,
    end: geo::Coord<f64>,
    center: Point<f64>,
    radius: f64,
    sweep: f64,
) -> Vec<PathSegment> {
    let segment_count = (sweep.abs() / std::f64::consts::FRAC_PI_2).ceil().max(1.0) as usize;
    let start_angle = (start.y - center.y()).atan2(start.x - center.x());
    let step = sweep / segment_count as f64;
    (0..segment_count)
        .map(|index| {
            let angle_start = start_angle + step * index as f64;
            let angle_end = angle_start + step;
            let arc_start = if index == 0 {
                point_mm(start)
            } else {
                circle_point(center, radius, angle_start)
            };
            let arc_end = if index + 1 == segment_count {
                point_mm(end)
            } else {
                circle_point(center, radius, angle_end)
            };
            let direction = step.signum();
            let control_length = radius * 4.0 / 3.0 * (step.abs() / 4.0).tan();
            let control_start = PointMm {
                x: arc_start.x + control_length * -angle_start.sin() * direction,
                y: arc_start.y + control_length * angle_start.cos() * direction,
            };
            let control_end = PointMm {
                x: arc_end.x + control_length * angle_end.sin() * direction,
                y: arc_end.y - control_length * angle_end.cos() * direction,
            };
            PathSegment::CubicBezier {
                start: arc_start,
                control_start,
                control_end,
                end: arc_end,
            }
        })
        .collect()
}

fn circle_curve_path(center: Point<f64>, radius: f64, counter_clockwise: bool) -> ClosedPath {
    let direction = if counter_clockwise { 1.0 } else { -1.0 };
    let sweep = direction * std::f64::consts::FRAC_PI_2;
    let segments = (0..4)
        .map(|index| {
            let angle_start = sweep * index as f64;
            let angle_end = angle_start + sweep;
            let start = circle_point(center, radius, angle_start);
            let end = circle_point(center, radius, angle_end);
            let control_length = radius * 4.0 / 3.0 * (sweep.abs() / 4.0).tan();
            PathSegment::CubicBezier {
                start,
                control_start: PointMm {
                    x: start.x + control_length * -angle_start.sin() * direction,
                    y: start.y + control_length * angle_start.cos() * direction,
                },
                control_end: PointMm {
                    x: end.x + control_length * angle_end.sin() * direction,
                    y: end.y - control_length * angle_end.cos() * direction,
                },
                end,
            }
        })
        .collect();
    ClosedPath::try_new(segments).expect("a finite circular Bezier path is closed")
}

fn circle_point(center: Point<f64>, radius: f64, angle: f64) -> PointMm {
    PointMm {
        x: center.x() + radius * angle.cos(),
        y: center.y() + radius * angle.sin(),
    }
}

fn point_mm(coordinate: geo::Coord<f64>) -> PointMm {
    PointMm {
        x: coordinate.x,
        y: coordinate.y,
    }
}

fn line_curve_path(ring: &LineString<f64>) -> Result<ClosedPath, RasterError> {
    let mut coordinates = ring.coords().copied().collect::<Vec<_>>();
    if coordinates.len() > 1 && coordinates.first() == coordinates.last() {
        coordinates.pop();
    }
    if coordinates.len() < 3 {
        return Err(RasterError::NoUsableContour);
    }
    let segments = coordinates
        .iter()
        .enumerate()
        .map(|(index, start)| PathSegment::Line {
            start: point_mm(*start),
            end: point_mm(coordinates[(index + 1) % coordinates.len()]),
        })
        .collect();
    ClosedPath::try_new(segments).map_err(|_| RasterError::NoUsableContour)
}

fn signed_ring_area(ring: &LineString<f64>) -> f64 {
    let coordinates = ring.coords().collect::<Vec<_>>();
    coordinates
        .iter()
        .zip(coordinates.iter().cycle().skip(1))
        .take(coordinates.len())
        .map(|(first, second)| first.x * second.y - second.x * first.y)
        .sum::<f64>()
        / 2.0
}

fn validate_hole_settings(settings: HolePlacementSettings) -> Result<(), RasterError> {
    if !settings.diameter_mm.is_finite()
        || settings.diameter_mm < 0.0
        || !settings.edge_clearance_mm.is_finite()
        || settings.edge_clearance_mm <= 0.0
        || !settings.smoothing_mm.is_finite()
        || settings.smoothing_mm < 0.0
    {
        return Err(RasterError::InvalidHoleSettings);
    }
    Ok(())
}

fn threshold_alpha(mask: &AlphaMask, alpha_threshold: u8) -> Result<Vec<u8>, RasterError> {
    #[cfg(feature = "opencv")]
    {
        use opencv::core::{AlgorithmHint, BORDER_DEFAULT, CV_8UC1, Mat, Scalar, Size};
        use opencv::prelude::{MatTraitConstManual, MatTraitManual};

        let rows = i32::try_from(mask.height).map_err(|_| RasterError::DimensionsTooLarge)?;
        let columns = i32::try_from(mask.width).map_err(|_| RasterError::DimensionsTooLarge)?;
        let mut source = Mat::new_rows_cols_with_default(rows, columns, CV_8UC1, Scalar::all(0.0))
            .map_err(|error| RasterError::ImageProcessing(error.to_string()))?;
        source
            .data_bytes_mut()
            .map_err(|error| RasterError::ImageProcessing(error.to_string()))?
            .copy_from_slice(&mask.values);

        let mut softened = Mat::default();
        opencv::imgproc::gaussian_blur(
            &source,
            &mut softened,
            Size::new(3, 3),
            0.0,
            0.0,
            BORDER_DEFAULT,
            AlgorithmHint::ALGO_HINT_DEFAULT,
        )
        .map_err(|error| RasterError::ImageProcessing(error.to_string()))?;
        let mut binary = Mat::default();
        opencv::imgproc::threshold(
            &softened,
            &mut binary,
            f64::from(alpha_threshold),
            255.0,
            opencv::imgproc::THRESH_BINARY,
        )
        .map_err(|error| RasterError::ImageProcessing(error.to_string()))?;
        binary
            .data_bytes()
            .map(|values| values.to_vec())
            .map_err(|error| RasterError::ImageProcessing(error.to_string()))
    }

    #[cfg(not(feature = "opencv"))]
    {
        Ok(mask
            .values
            .iter()
            .map(|alpha| if *alpha > alpha_threshold { 255 } else { 0 })
            .collect())
    }
}

fn smooth_binary_alpha(values: &[u8], width: u32, height: u32) -> Result<Vec<u8>, RasterError> {
    if width == 0 || height == 0 {
        return Ok(Vec::new());
    }

    #[cfg(feature = "opencv")]
    {
        use opencv::core::{AlgorithmHint, BORDER_DEFAULT, CV_8UC1, Mat, Scalar, Size};
        use opencv::prelude::{MatTraitConstManual, MatTraitManual};

        let rows = i32::try_from(height).map_err(|_| RasterError::DimensionsTooLarge)?;
        let columns = i32::try_from(width).map_err(|_| RasterError::DimensionsTooLarge)?;
        let mut source = Mat::new_rows_cols_with_default(rows, columns, CV_8UC1, Scalar::all(0.0))
            .map_err(|error| RasterError::ImageProcessing(error.to_string()))?;
        source
            .data_bytes_mut()
            .map_err(|error| RasterError::ImageProcessing(error.to_string()))?
            .copy_from_slice(values);

        let mut softened = Mat::default();
        opencv::imgproc::gaussian_blur(
            &source,
            &mut softened,
            Size::new(3, 3),
            0.0,
            0.0,
            BORDER_DEFAULT,
            AlgorithmHint::ALGO_HINT_DEFAULT,
        )
        .map_err(|error| RasterError::ImageProcessing(error.to_string()))?;
        softened
            .data_bytes()
            .map(|bytes| bytes.to_vec())
            .map_err(|error| RasterError::ImageProcessing(error.to_string()))
    }

    #[cfg(not(feature = "opencv"))]
    {
        let width = usize::try_from(width).map_err(|_| RasterError::DimensionsTooLarge)?;
        let height = usize::try_from(height).map_err(|_| RasterError::DimensionsTooLarge)?;
        let mut softened = vec![0; values.len()];
        const WEIGHTS: [u16; 3] = [1, 2, 1];
        for y in 0..height {
            for x in 0..width {
                let mut sum = 0_u16;
                for (dy, wy) in WEIGHTS.iter().enumerate() {
                    for (dx, wx) in WEIGHTS.iter().enumerate() {
                        let source_x = x as isize + dx as isize - 1;
                        let source_y = y as isize + dy as isize - 1;
                        if source_x < 0
                            || source_y < 0
                            || source_x >= width as isize
                            || source_y >= height as isize
                        {
                            continue;
                        }
                        let source_index = source_y as usize * width + source_x as usize;
                        sum += u16::from(values[source_index]) * wx * wy;
                    }
                }
                softened[y * width + x] = ((sum + 8) / 16) as u8;
            }
        }
        Ok(softened)
    }
}

fn top_boundary_intersection(outline: &MultiPolygon<f64>, x: f64) -> Option<f64> {
    const EPSILON: f64 = 1.0e-9;
    let mut top: Option<f64> = None;

    for polygon in outline {
        for segment in polygon.exterior().lines() {
            let min_x = segment.start.x.min(segment.end.x);
            let max_x = segment.start.x.max(segment.end.x);
            if x < min_x - EPSILON || x > max_x + EPSILON {
                continue;
            }

            let delta_x = segment.end.x - segment.start.x;
            let y = if delta_x.abs() <= EPSILON {
                if (x - segment.start.x).abs() > EPSILON {
                    continue;
                }
                segment.start.y.min(segment.end.y)
            } else {
                let fraction = (x - segment.start.x) / delta_x;
                segment.start.y + fraction * (segment.end.y - segment.start.y)
            };
            top = Some(top.map_or(y, |current| current.min(y)));
        }
    }
    top
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RasterError {
    DimensionsTooLarge,
    AlphaLengthMismatch { expected: usize, actual: usize },
    InvalidPhysicalSize,
    InvalidOutlineSettings,
    NoTransparentEdge,
    NoUsableContour,
    InvalidHoleSettings,
    NoTopCenterIntersection,
    Contour(String),
    ImageProcessing(String),
}

impl fmt::Display for RasterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DimensionsTooLarge => formatter.write_str("raster dimensions are too large"),
            Self::AlphaLengthMismatch { expected, actual } => write!(
                formatter,
                "alpha channel length mismatch: expected {expected}, got {actual}"
            ),
            Self::InvalidPhysicalSize => {
                formatter.write_str("physical raster dimensions must be finite and positive")
            }
            Self::InvalidOutlineSettings => formatter.write_str("outline settings are invalid"),
            Self::NoTransparentEdge => {
                formatter.write_str("raster does not contain a transparent edge")
            }
            Self::NoUsableContour => formatter.write_str("no usable alpha contour was found"),
            Self::InvalidHoleSettings => formatter.write_str("hole settings are invalid"),
            Self::NoTopCenterIntersection => {
                formatter.write_str("outline does not intersect its top-center axis")
            }
            Self::Contour(message) => write!(formatter, "contour extraction failed: {message}"),
            Self::ImageProcessing(message) => {
                write!(formatter, "OpenCV alpha processing failed: {message}")
            }
        }
    }
}

impl Error for RasterError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_a_transparent_edge_and_content_bounds() {
        let mask = AlphaMask::try_new(3, 3, vec![0, 0, 0, 0, 128, 255, 0, 255, 0]).unwrap();

        let stats = mask.stats();
        assert!(stats.has_transparent_edge());
        assert_eq!(stats.transparent_pixels, 6);
        assert_eq!(stats.partial_pixels, 1);
        assert_eq!(stats.opaque_pixels, 2);
        assert_eq!(
            stats.content_bounds,
            Some(PixelBounds {
                left: 1,
                top: 1,
                right: 2,
                bottom: 2,
            })
        );
    }

    #[test]
    fn rejects_a_wrong_alpha_channel_length() {
        let error = AlphaMask::try_new(2, 2, vec![0, 255]).unwrap_err();
        assert_eq!(
            error,
            RasterError::AlphaLengthMismatch {
                expected: 4,
                actual: 2,
            }
        );
    }

    #[test]
    fn antialiased_threshold_creates_a_one_pixel_transparent_feather() {
        let mut alpha = vec![0; 49];
        for y in 2..=4 {
            for x in 2..=4 {
                alpha[y * 7 + x] = 255;
            }
        }
        let mask = AlphaMask::try_new(7, 7, alpha).unwrap();

        let softened = mask.antialiased_threshold(96).unwrap();

        assert_eq!(softened[3 * 7 + 3], 255);
        assert!((180..=205).contains(&softened[3 * 7 + 2]));
        assert!((50..=80).contains(&softened[3 * 7 + 1]));
        assert_eq!(softened[3 * 7], 0);
    }

    #[test]
    fn fully_opaque_image_has_no_transparent_edge() {
        let mask = AlphaMask::try_new(2, 2, vec![255; 4]).unwrap();
        assert!(!mask.stats().has_transparent_edge());
    }

    #[test]
    fn offsets_an_alpha_contour_by_two_millimeters() {
        use geo::BoundingRect;

        let mut values = vec![0; 25];
        for y in 1..=3 {
            for x in 1..=3 {
                values[y * 5 + x] = 255;
            }
        }
        let mask = AlphaMask::try_new(5, 5, values).unwrap();
        let outline = mask
            .offset_outline(
                5.0,
                5.0,
                OutlineSettings {
                    alpha_threshold: 127,
                    offset_mm: 2.0,
                    tool_diameter_mm: 2.0,
                    smoothing_mm: 0.02,
                    minimum_component_area_mm2: 0.0,
                },
            )
            .unwrap();

        let bounds = outline.bounding_rect().unwrap();
        assert!(
            (bounds.min().x + 1.0).abs() < 1.0e-6,
            "unexpected bounds: {bounds:?}"
        );
        assert!((bounds.min().y + 1.0).abs() < 1.0e-6);
        assert!((bounds.max().x - 6.0).abs() < 1.0e-6);
        assert!((bounds.max().y - 6.0).abs() < 1.0e-6);
    }

    #[test]
    fn traces_an_alpha_contour_without_outer_expansion() {
        use geo::BoundingRect;

        let mut values = vec![0; 5 * 5];
        for y in 1..=3 {
            for x in 1..=3 {
                values[y * 5 + x] = 255;
            }
        }
        let mask = AlphaMask::try_new(5, 5, values).unwrap();
        let contour = mask
            .trace_contour(
                5.0,
                5.0,
                OutlineSettings {
                    alpha_threshold: 127,
                    offset_mm: 0.0,
                    tool_diameter_mm: 0.0,
                    smoothing_mm: 0.02,
                    minimum_component_area_mm2: 0.0,
                },
            )
            .unwrap();

        let bounds = contour.bounding_rect().unwrap();
        assert!((bounds.min().x - 1.0).abs() < 1.0e-6);
        assert!((bounds.min().y - 1.0).abs() < 1.0e-6);
        assert!((bounds.max().x - 4.0).abs() < 1.0e-6);
        assert!((bounds.max().y - 4.0).abs() < 1.0e-6);
    }

    #[test]
    fn keeps_only_exterior_alpha_contours() {
        let mut values = vec![0; 12 * 12];
        for y in 1..=10 {
            for x in 1..=10 {
                values[y * 12 + x] = 255;
            }
        }
        for y in 4..=7 {
            for x in 4..=7 {
                values[y * 12 + x] = 0;
            }
        }
        let mask = AlphaMask::try_new(12, 12, values).unwrap();
        let outline = mask
            .offset_outline(
                12.0,
                12.0,
                OutlineSettings {
                    alpha_threshold: 127,
                    offset_mm: 0.5,
                    tool_diameter_mm: 2.0,
                    smoothing_mm: 0.02,
                    minimum_component_area_mm2: 0.0,
                },
            )
            .unwrap();

        assert!(
            outline.iter().all(|polygon| polygon.interiors().is_empty()),
            "transparent pockets inside the artwork must not become cut paths"
        );
    }

    #[test]
    fn rejects_negative_smoothing() {
        let mask = AlphaMask::try_new(3, 3, vec![0, 0, 0, 0, 255, 0, 0, 0, 0]).unwrap();
        let error = mask
            .offset_outline(
                3.0,
                3.0,
                OutlineSettings {
                    alpha_threshold: 127,
                    offset_mm: 2.0,
                    tool_diameter_mm: 2.0,
                    smoothing_mm: -0.01,
                    minimum_component_area_mm2: 0.0,
                },
            )
            .unwrap_err();

        assert_eq!(error, RasterError::InvalidOutlineSettings);
    }

    #[test]
    fn closes_a_groove_narrower_than_the_tool_diameter() {
        use geo::{Contains, LineString, coord};

        let narrow = MultiPolygon::from(Polygon::new(
            LineString::from(vec![
                coord! { x: 0.0, y: 0.0 },
                coord! { x: 10.0, y: 0.0 },
                coord! { x: 10.0, y: 10.0 },
                coord! { x: 5.5, y: 10.0 },
                coord! { x: 5.5, y: 4.0 },
                coord! { x: 4.5, y: 4.0 },
                coord! { x: 4.5, y: 10.0 },
                coord! { x: 0.0, y: 10.0 },
                coord! { x: 0.0, y: 0.0 },
            ]),
            Vec::new(),
        ));
        let closed = close_narrow_grooves(&narrow, 2.0, 0.0);

        assert!(closed.contains(&Point::new(5.0, 8.0)));
    }

    #[test]
    fn keeps_a_groove_wider_than_the_tool_diameter() {
        use geo::{Contains, LineString, coord};

        let wide = MultiPolygon::from(Polygon::new(
            LineString::from(vec![
                coord! { x: 0.0, y: 0.0 },
                coord! { x: 10.0, y: 0.0 },
                coord! { x: 10.0, y: 10.0 },
                coord! { x: 6.5, y: 10.0 },
                coord! { x: 6.5, y: 4.0 },
                coord! { x: 3.5, y: 4.0 },
                coord! { x: 3.5, y: 10.0 },
                coord! { x: 0.0, y: 10.0 },
                coord! { x: 0.0, y: 0.0 },
            ]),
            Vec::new(),
        ));
        let closed = close_narrow_grooves(&wide, 2.0, 0.0);

        assert!(!closed.contains(&Point::new(5.0, 8.0)));
    }

    #[test]
    fn combines_a_top_center_hole_with_the_outline() {
        use geo::{Contains, Rect, coord};

        let outline = MultiPolygon::from(Rect::new(
            coord! { x: 0.0, y: 0.0 },
            coord! { x: 10.0, y: 10.0 },
        ));
        let result = combine_top_center_hole(
            &outline,
            HolePlacementSettings {
                diameter_mm: 3.0,
                edge_clearance_mm: 2.0,
                smoothing_mm: 0.02,
            },
        )
        .unwrap();

        let hole_center = result.hole_center.unwrap();
        assert_eq!(hole_center, Point::new(5.0, 0.0));
        assert!(!result.geometry.contains(&hole_center));
        let bounds = result.geometry.bounding_rect().unwrap();
        assert!((bounds.min().y + 3.5).abs() < 1.0e-6);
        let actual_edge_clearance = hole_center.y() - bounds.min().y - 1.5;
        assert!((actual_edge_clearance - 2.0).abs() < 1.0e-6);
        assert_eq!(result.geometry.0.len(), 1);
        assert_eq!(result.geometry.0[0].interiors().len(), 1);
    }

    #[test]
    fn rounded_square_hole_geometry_uses_fewer_vertices_than_round_buffer() {
        use geo::{BooleanOps, Contains, Rect, coord};

        let outline = MultiPolygon::from(Rect::new(
            coord! { x: 0.0, y: 0.0 },
            coord! { x: 10.0, y: 10.0 },
        ));
        let settings = HolePlacementSettings {
            diameter_mm: 3.0,
            edge_clearance_mm: 2.0,
            smoothing_mm: 0.02,
        };
        let result = combine_top_center_hole(&outline, settings).unwrap();

        let hole_center = result.hole_center.unwrap();
        let legacy_geometry = outline
            .union(&hole_center.buffer(settings.diameter_mm / 2.0 + settings.edge_clearance_mm))
            .difference(&hole_center.buffer(settings.diameter_mm / 2.0))
            .simplify(settings.smoothing_mm);
        let count_vertices = |geometry: &MultiPolygon<f64>| {
            geometry
                .iter()
                .map(|polygon| {
                    polygon.exterior().coords().count() - 1
                        + polygon
                            .interiors()
                            .iter()
                            .map(|ring| ring.coords().count() - 1)
                            .sum::<usize>()
                })
                .sum::<usize>()
        };

        assert_eq!(hole_center, Point::new(5.0, 0.0));
        assert!(result.geometry.contains(&Point::new(5.0, -3.4)));
        assert!(!result.geometry.contains(&hole_center));
        assert!(count_vertices(&result.geometry) < count_vertices(&legacy_geometry));
    }

    #[test]
    fn added_hole_ring_keeps_the_opening_and_uses_fewer_vertices() {
        use geo::{BooleanOps, Contains, Rect, coord};

        let outline = MultiPolygon::from(Rect::new(
            coord! { x: 0.0, y: 0.0 },
            coord! { x: 10.0, y: 10.0 },
        ));
        let settings = HolePlacementSettings {
            diameter_mm: 3.0,
            edge_clearance_mm: 2.0,
            smoothing_mm: 0.02,
        };
        let ring = add_top_center_hole_ring(&outline, settings).unwrap();
        let hole_center = ring.hole_center.unwrap();
        let legacy_ring = hole_center
            .buffer(settings.diameter_mm / 2.0 + settings.edge_clearance_mm)
            .difference(&hole_center.buffer(settings.diameter_mm / 2.0))
            .simplify(settings.smoothing_mm);
        let count_vertices = |geometry: &MultiPolygon<f64>| {
            geometry
                .iter()
                .map(|polygon| {
                    polygon.exterior().coords().count() - 1
                        + polygon
                            .interiors()
                            .iter()
                            .map(|ring| ring.coords().count() - 1)
                            .sum::<usize>()
                })
                .sum::<usize>()
        };
        let bounds = ring.geometry.bounding_rect().unwrap();

        assert_eq!(hole_center, Point::new(5.0, 0.0));
        assert!(!ring.geometry.contains(&hole_center));
        assert!(ring.geometry.contains(&Point::new(5.0, -2.0)));
        assert!((bounds.min().x - 1.5).abs() < 1.0e-6);
        assert!((bounds.max().x - 8.5).abs() < 1.0e-6);
        assert!((bounds.min().y + 3.5).abs() < 1.0e-6);
        assert!((bounds.max().y - 3.5).abs() < 1.0e-6);
        assert!(count_vertices(&ring.geometry) < count_vertices(&legacy_ring));
    }

    #[test]
    fn added_hole_ring_exports_four_bezier_arcs_per_circle() {
        use cdr_domain::PathSegment;
        use geo::{Rect, coord};

        let outline = MultiPolygon::from(Rect::new(
            coord! { x: 0.0, y: 0.0 },
            coord! { x: 10.0, y: 10.0 },
        ));
        let result = add_top_center_hole_ring(
            &outline,
            HolePlacementSettings {
                diameter_mm: 3.0,
                edge_clearance_mm: 2.0,
                smoothing_mm: 0.02,
            },
        )
        .unwrap();
        let curved = result.curved_geometry.as_ref().unwrap();

        assert_eq!(result.hole_center, Some(Point::new(5.0, 0.0)));
        assert_eq!(curved.len(), 1);
        assert_eq!(curved[0].exterior.segments().len(), 4);
        assert_eq!(curved[0].interiors.len(), 1);
        assert_eq!(curved[0].interiors[0].segments().len(), 4);
        assert!(
            curved[0]
                .exterior
                .segments()
                .iter()
                .all(|segment| matches!(segment, PathSegment::CubicBezier { .. }))
        );
        assert!(
            curved[0].interiors[0]
                .segments()
                .iter()
                .all(|segment| matches!(segment, PathSegment::CubicBezier { .. }))
        );
        let bounds = result.geometry.bounding_rect().unwrap();
        assert!((bounds.min().x - 1.5).abs() < 1.0e-6);
        assert!((bounds.max().x - 8.5).abs() < 1.0e-6);
        assert!((bounds.min().y + 3.5).abs() < 1.0e-6);
        assert!((bounds.max().y - 3.5).abs() < 1.0e-6);
    }

    #[test]
    fn merged_hole_keeps_a_bezier_opening_and_round_support_arc() {
        use cdr_domain::PathSegment;
        use geo::{Rect, coord};

        let outline = MultiPolygon::from(Rect::new(
            coord! { x: 0.0, y: 0.0 },
            coord! { x: 10.0, y: 10.0 },
        ));
        let result = combine_top_center_hole(
            &outline,
            HolePlacementSettings {
                diameter_mm: 3.0,
                edge_clearance_mm: 2.0,
                smoothing_mm: 0.02,
            },
        )
        .unwrap();
        let curved = result.curved_geometry.as_ref().unwrap();

        assert_eq!(result.hole_center, Some(Point::new(5.0, 0.0)));
        assert_eq!(curved.len(), 1);
        assert!(
            curved[0]
                .exterior
                .segments()
                .iter()
                .any(|segment| matches!(segment, PathSegment::CubicBezier { .. }))
        );
        assert_eq!(curved[0].interiors.len(), 1);
        assert_eq!(curved[0].interiors[0].segments().len(), 4);
        assert!(
            curved[0].interiors[0]
                .segments()
                .iter()
                .all(|segment| matches!(segment, PathSegment::CubicBezier { .. }))
        );
        let bounds = result.geometry.bounding_rect().unwrap();
        assert!((bounds.min().y + 3.5).abs() < 1.0e-6);
    }

    #[test]
    fn zero_hole_diameter_keeps_only_the_outline() {
        use geo::{Rect, coord};

        let outline = MultiPolygon::from(Rect::new(
            coord! { x: 0.0, y: 0.0 },
            coord! { x: 10.0, y: 10.0 },
        ));
        let result = combine_top_center_hole(
            &outline,
            HolePlacementSettings {
                diameter_mm: 0.0,
                edge_clearance_mm: 2.0,
                smoothing_mm: 0.02,
            },
        )
        .unwrap();

        assert_eq!(result.geometry, outline);
        assert_eq!(result.hole_center, None);
    }
}
