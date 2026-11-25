//! Pure G-code generation with safety checks. No I/O, so it can be unit tested
//! without a printer.

use anyhow::{bail, Result};

/// Largest single relative move allowed from the app, in millimetres.
pub const MAX_JOG_MM: f64 = 100.0;
/// Largest single filament move allowed from the app, in millimetres.
pub const MAX_FILAMENT_MM: f64 = 200.0;
/// Lowest nozzle temperature at which filament may be moved.
pub const MIN_EXTRUDE_TEMP_C: f64 = 170.0;
/// Highest nozzle temperature the app will request.
pub const MAX_NOZZLE_TEMP_C: f64 = 300.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    X,
    Y,
    Z,
}

impl Axis {
    pub fn letter(self) -> char {
        match self {
            Axis::X => 'X',
            Axis::Y => 'Y',
            Axis::Z => 'Z',
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s.to_ascii_uppercase().as_str() {
            "X" => Ok(Axis::X),
            "Y" => Ok(Axis::Y),
            "Z" => Ok(Axis::Z),
            other => bail!("unknown axis '{other}', use X, Y or Z"),
        }
    }
}

/// Relative jog of one axis. Returns G-code lines to send in order.
/// Uses relative positioning (G91) and restores absolute (G90) afterwards.
pub fn jog(axis: Axis, distance_mm: f64, feedrate_mm_min: f64) -> Result<Vec<String>> {
    check_finite(distance_mm, "distance")?;
    if distance_mm == 0.0 {
        bail!("distance must not be zero");
    }
    if distance_mm.abs() > MAX_JOG_MM {
        bail!("distance {distance_mm} mm exceeds the per-move limit of {MAX_JOG_MM} mm");
    }
    if !(1.0..=10_000.0).contains(&feedrate_mm_min) {
        bail!("feedrate must be between 1 and 10000 mm/min");
    }
    Ok(vec![
        "G91".to_string(),
        format!("G1 {}{:.2} F{:.0}", axis.letter(), distance_mm, feedrate_mm_min),
        "G90".to_string(),
    ])
}

/// Home the given axes. Empty slice homes all axes.
pub fn home(axes: &[Axis]) -> Vec<String> {
    if axes.is_empty() {
        return vec!["G28".to_string()];
    }
    let letters: String = axes.iter().map(|a| format!(" {}", a.letter())).collect();
    vec![format!("G28{letters}")]
}

/// Heat the nozzle to a target temperature without waiting.
pub fn set_nozzle_temp(target_c: f64) -> Result<Vec<String>> {
    check_finite(target_c, "temperature")?;
    if !(0.0..=MAX_NOZZLE_TEMP_C).contains(&target_c) {
        bail!("nozzle temperature must be between 0 and {MAX_NOZZLE_TEMP_C} C");
    }
    Ok(vec![format!("M104 S{:.0}", target_c)])
}

/// Move filament through the extruder. Positive `length_mm` = load/feed,
/// negative = retract/unload. Refuses to run unless the nozzle is already hot
/// enough, which prevents grinding the filament in a cold nozzle.
pub fn move_filament(length_mm: f64, feedrate_mm_min: f64, nozzle_temp_c: f64) -> Result<Vec<String>> {
    check_finite(length_mm, "length")?;
    if length_mm == 0.0 {
        bail!("length must not be zero");
    }
    if length_mm.abs() > MAX_FILAMENT_MM {
        bail!("length {length_mm} mm exceeds the per-move limit of {MAX_FILAMENT_MM} mm");
    }
    if !(1.0..=1_000.0).contains(&feedrate_mm_min) {
        bail!("filament feedrate must be between 1 and 1000 mm/min");
    }
    if nozzle_temp_c < MIN_EXTRUDE_TEMP_C {
        bail!(
            "nozzle is at {nozzle_temp_c:.0} C, needs at least {MIN_EXTRUDE_TEMP_C:.0} C before moving filament"
        );
    }
    Ok(vec![
        "M83".to_string(), // relative extrusion
        format!("G1 E{:.2} F{:.0}", length_mm, feedrate_mm_min),
        "M82".to_string(), // back to absolute extrusion
    ])
}

/// Part-cooling fan at a percentage (0 = off, 100 = full). P1 is the part fan.
pub fn fan(percent: u8) -> Result<Vec<String>> {
    if percent > 100 {
        bail!("fan speed must be between 0 and 100 %");
    }
    let s = (f64::from(percent) * 255.0 / 100.0).round() as u32;
    Ok(vec![format!("M106 P1 S{s}")])
}

fn check_finite(v: f64, what: &str) -> Result<()> {
    if !v.is_finite() {
        bail!("{what} must be a finite number");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jog_builds_relative_move() {
        let lines = jog(Axis::X, 10.0, 3000.0).unwrap();
        assert_eq!(lines, vec!["G91", "G1 X10.00 F3000", "G90"]);
    }

    #[test]
    fn jog_rejects_large_and_zero_moves() {
        assert!(jog(Axis::Z, 150.0, 600.0).is_err());
        assert!(jog(Axis::Z, 0.0, 600.0).is_err());
        assert!(jog(Axis::Z, f64::NAN, 600.0).is_err());
        assert!(jog(Axis::Z, 5.0, 0.0).is_err());
    }

    #[test]
    fn home_all_and_some() {
        assert_eq!(home(&[]), vec!["G28"]);
        assert_eq!(home(&[Axis::X, Axis::Y]), vec!["G28 X Y"]);
    }

    #[test]
    fn filament_needs_hot_nozzle() {
        assert!(move_filament(50.0, 300.0, 25.0).is_err());
        assert!(move_filament(50.0, 300.0, 220.0).is_ok());
    }

    #[test]
    fn filament_unload_is_negative() {
        let lines = move_filament(-60.0, 300.0, 220.0).unwrap();
        assert_eq!(lines[1], "G1 E-60.00 F300");
    }

    #[test]
    fn filament_limit_enforced() {
        assert!(move_filament(500.0, 300.0, 220.0).is_err());
    }

    #[test]
    fn axis_parse() {
        assert_eq!(Axis::parse("x").unwrap(), Axis::X);
        assert!(Axis::parse("E").is_err());
    }
}
