#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fmt;

const CONTINUITY_EPSILON_MM: f64 = 1.0e-9;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PointMm {
    pub x: f64,
    pub y: f64,
}

impl PointMm {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PathSegment {
    Line {
        start: PointMm,
        end: PointMm,
    },
    CubicBezier {
        start: PointMm,
        control_start: PointMm,
        control_end: PointMm,
        end: PointMm,
    },
}

impl PathSegment {
    pub const fn start(&self) -> PointMm {
        match self {
            Self::Line { start, .. } | Self::CubicBezier { start, .. } => *start,
        }
    }

    pub const fn end(&self) -> PointMm {
        match self {
            Self::Line { end, .. } | Self::CubicBezier { end, .. } => *end,
        }
    }

    fn has_only_finite_points(&self) -> bool {
        match self {
            Self::Line { start, end } => start.is_finite() && end.is_finite(),
            Self::CubicBezier {
                start,
                control_start,
                control_end,
                end,
            } => {
                start.is_finite()
                    && control_start.is_finite()
                    && control_end.is_finite()
                    && end.is_finite()
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClosedPath {
    segments: Vec<PathSegment>,
}

impl ClosedPath {
    pub fn try_new(segments: Vec<PathSegment>) -> Result<Self, DomainError> {
        if segments.is_empty() {
            return Err(DomainError::NoSegments);
        }

        if segments
            .iter()
            .any(|segment| !segment.has_only_finite_points())
        {
            return Err(DomainError::NonFiniteCoordinate);
        }

        for pair in segments.windows(2) {
            if !points_are_continuous(pair[0].end(), pair[1].start()) {
                return Err(DomainError::DiscontinuousPath);
            }
        }

        let first = segments.first().expect("segments are not empty").start();
        let last = segments.last().expect("segments are not empty").end();
        if !points_are_continuous(last, first) {
            return Err(DomainError::OpenPath);
        }

        Ok(Self { segments })
    }

    pub fn try_from_vertices(mut vertices: Vec<PointMm>) -> Result<Self, DomainError> {
        if vertices.iter().any(|point| !point.is_finite()) {
            return Err(DomainError::NonFiniteCoordinate);
        }

        if vertices.first() == vertices.last() {
            vertices.pop();
        }
        if vertices.len() < 3 {
            return Err(DomainError::TooFewVertices);
        }

        let mut segments = Vec::with_capacity(vertices.len());
        for index in 0..vertices.len() {
            segments.push(PathSegment::Line {
                start: vertices[index],
                end: vertices[(index + 1) % vertices.len()],
            });
        }
        Self::try_new(segments)
    }

    pub fn segments(&self) -> &[PathSegment] {
        &self.segments
    }
}

fn points_are_continuous(left: PointMm, right: PointMm) -> bool {
    (left.x - right.x).abs() <= CONTINUITY_EPSILON_MM
        && (left.y - right.y).abs() <= CONTINUITY_EPSILON_MM
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PositiveMm(f64);

impl PositiveMm {
    pub fn try_new(value: f64) -> Result<Self, DomainError> {
        if !value.is_finite() {
            return Err(DomainError::NonFiniteMeasurement);
        }
        if value <= 0.0 {
            return Err(DomainError::NonPositiveMeasurement);
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> f64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HoleSettings {
    pub diameter: PositiveMm,
    pub edge_clearance: PositiveMm,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hole {
    pub center: PointMm,
    pub diameter: PositiveMm,
}

impl Hole {
    pub fn try_new(center: PointMm, diameter: PositiveMm) -> Result<Self, DomainError> {
        if !center.is_finite() {
            return Err(DomainError::NonFiniteCoordinate);
        }
        Ok(Self { center, diameter })
    }
}

impl HoleSettings {
    pub const fn new(diameter: PositiveMm, edge_clearance: PositiveMm) -> Self {
        Self {
            diameter,
            edge_clearance,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DomainError {
    NonFiniteCoordinate,
    NoSegments,
    TooFewVertices,
    DiscontinuousPath,
    OpenPath,
    NonFiniteMeasurement,
    NonPositiveMeasurement,
}

impl fmt::Display for DomainError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::NonFiniteCoordinate => "path coordinates must be finite",
            Self::NoSegments => "a closed path requires at least one segment",
            Self::TooFewVertices => "a closed path requires at least three vertices",
            Self::DiscontinuousPath => "adjacent path segments must share an endpoint",
            Self::OpenPath => "the final path segment must end at the first point",
            Self::NonFiniteMeasurement => "measurements must be finite",
            Self::NonPositiveMeasurement => "measurements must be greater than zero",
        };
        formatter.write_str(message)
    }
}

impl Error for DomainError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_repeated_closing_vertex() {
        let path = ClosedPath::try_from_vertices(vec![
            PointMm::new(0.0, 0.0),
            PointMm::new(10.0, 0.0),
            PointMm::new(0.0, 10.0),
            PointMm::new(0.0, 0.0),
        ])
        .unwrap();

        assert_eq!(path.segments().len(), 3);
    }

    #[test]
    fn rejects_too_few_vertices() {
        let error =
            ClosedPath::try_from_vertices(vec![PointMm::new(0.0, 0.0), PointMm::new(10.0, 0.0)])
                .unwrap_err();

        assert_eq!(error, DomainError::TooFewVertices);
    }

    #[test]
    fn rejects_non_finite_coordinates() {
        let error = ClosedPath::try_from_vertices(vec![
            PointMm::new(0.0, 0.0),
            PointMm::new(f64::NAN, 0.0),
            PointMm::new(0.0, 10.0),
        ])
        .unwrap_err();

        assert_eq!(error, DomainError::NonFiniteCoordinate);
    }

    #[test]
    fn accepts_a_closed_cubic_bezier() {
        let origin = PointMm::new(0.0, 0.0);
        let path = ClosedPath::try_new(vec![PathSegment::CubicBezier {
            start: origin,
            control_start: PointMm::new(10.0, 0.0),
            control_end: PointMm::new(0.0, 10.0),
            end: origin,
        }])
        .unwrap();

        assert_eq!(path.segments().len(), 1);
    }

    #[test]
    fn rejects_discontinuous_segments() {
        let error = ClosedPath::try_new(vec![
            PathSegment::Line {
                start: PointMm::new(0.0, 0.0),
                end: PointMm::new(1.0, 0.0),
            },
            PathSegment::Line {
                start: PointMm::new(2.0, 0.0),
                end: PointMm::new(0.0, 0.0),
            },
        ])
        .unwrap_err();

        assert_eq!(error, DomainError::DiscontinuousPath);
    }

    #[test]
    fn rejects_an_open_path() {
        let error = ClosedPath::try_new(vec![
            PathSegment::Line {
                start: PointMm::new(0.0, 0.0),
                end: PointMm::new(1.0, 0.0),
            },
            PathSegment::Line {
                start: PointMm::new(1.0, 0.0),
                end: PointMm::new(2.0, 0.0),
            },
        ])
        .unwrap_err();

        assert_eq!(error, DomainError::OpenPath);
    }

    #[test]
    fn positive_measurements_reject_zero() {
        let error = PositiveMm::try_new(0.0).unwrap_err();
        assert_eq!(error, DomainError::NonPositiveMeasurement);
    }

    #[test]
    fn creates_a_positioned_hole() {
        let diameter = PositiveMm::try_new(3.0).unwrap();
        let hole = Hole::try_new(PointMm::new(10.0, 20.0), diameter).unwrap();

        assert_eq!(hole.center, PointMm::new(10.0, 20.0));
        assert_eq!(hole.diameter.get(), 3.0);
    }

    #[test]
    fn positioned_hole_rejects_a_non_finite_center() {
        let diameter = PositiveMm::try_new(3.0).unwrap();
        let error = Hole::try_new(PointMm::new(f64::INFINITY, 20.0), diameter).unwrap_err();

        assert_eq!(error, DomainError::NonFiniteCoordinate);
    }

    #[test]
    fn stores_confirmed_hole_settings() {
        let settings = HoleSettings::new(
            PositiveMm::try_new(3.0).unwrap(),
            PositiveMm::try_new(2.0).unwrap(),
        );

        assert_eq!(settings.diameter.get(), 3.0);
        assert_eq!(settings.edge_clearance.get(), 2.0);
    }
}
