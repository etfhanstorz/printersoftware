//! Shape paths built from short straight moves. Each move is relative, so the
//! path starts and ends where the head is now.

use anyhow::{Result, bail};

/// Largest circle radius the app will draw, in millimetres.
pub const MAX_RADIUS_MM: f64 = 20.0;

/// G-code that moves the X and Y axes around a circle of `radius_mm`, starting
/// and ending at the current position. The circle is drawn with straight segments.
pub fn circle(radius_mm: f64, segments: u32, feed_mm_min: f64) -> Result<Vec<String>> {
    if !radius_mm.is_finite() || radius_mm <= 0.0 || radius_mm > MAX_RADIUS_MM {
        bail!("radius must be between 0 and {MAX_RADIUS_MM} mm");
    }
    if !(8..=360).contains(&segments) {
        bail!("segments must be between 8 and 360");
    }
    if !(1.0..=10_000.0).contains(&feed_mm_min) {
        bail!("feedrate must be between 1 and 10000 mm/min");
    }

    // Points on the circle, starting at angle 0 from the current position.
    let point = |i: u32| {
        let theta = 2.0 * std::f64::consts::PI * f64::from(i) / f64::from(segments);
        (radius_mm * (theta.cos() - 1.0), radius_mm * theta.sin())
    };

    let mut lines = vec!["G91".to_string()];
    let mut prev = point(0);
    for i in 1..=segments {
        let p = point(i);
        let (dx, dy) = (p.0 - prev.0, p.1 - prev.1);
        lines.push(format!("G1 X{dx:.3} Y{dy:.3} F{feed_mm_min:.0}"));
        prev = p;
    }
    lines.push("G90".to_string());
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn circle_returns_to_start() {
        let lines = circle(10.0, 36, 3000.0).unwrap();
        let (mut sx, mut sy) = (0.0, 0.0);
        for line in lines.iter().filter(|l| l.starts_with("G1")) {
            for part in line.split_whitespace() {
                if let Some(v) = part.strip_prefix('X') {
                    sx += v.parse::<f64>().unwrap();
                }
                if let Some(v) = part.strip_prefix('Y') {
                    sy += v.parse::<f64>().unwrap();
                }
            }
        }
        assert!(sx.abs() < 0.01 && sy.abs() < 0.01, "path did not close: {sx}, {sy}");
    }

    #[test]
    fn circle_rejects_bad_input() {
        assert!(circle(0.0, 36, 3000.0).is_err());
        assert!(circle(50.0, 36, 3000.0).is_err());
        assert!(circle(10.0, 2, 3000.0).is_err());
    }
}
