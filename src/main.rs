use std::time::Duration;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

use a1_control::gcode::{self, Axis};
use a1_control::printer::{self, Printer};

/// Manual control for a Bambu A1 over LAN mode.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Printer IP address on your LAN
    #[arg(long, env = "A1_IP")]
    ip: String,
    /// Printer serial number (device ID); not needed for `discover`
    #[arg(long, env = "A1_SERIAL")]
    serial: Option<String>,
    /// Access code shown on the printer (LAN mode)
    #[arg(long, env = "A1_ACCESS_CODE")]
    access_code: String,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Find the printer's serial number by listening on the network
    Discover,
    /// Show current state and temperatures
    Status,
    /// Home one or more axes (all if none given), e.g. `home X Y`
    Home { axes: Vec<String> },
    /// Relative jog of one axis
    Jog {
        axis: String,
        #[arg(allow_hyphen_values = true)]
        distance_mm: f64,
        #[arg(long, default_value_t = 3000.0)]
        feed: f64,
    },
    /// Set nozzle target temperature (does not wait)
    Nozzle { celsius: f64 },
    /// Feed filament in (positive mm) — needs nozzle >= 170 C
    Load {
        #[arg(default_value_t = 50.0)]
        mm: f64,
        #[arg(long, default_value_t = 300.0)]
        feed: f64,
    },
    /// Pull filament back out (sends a negative move)
    Unload {
        #[arg(default_value_t = 60.0)]
        mm: f64,
        #[arg(long, default_value_t = 300.0)]
        feed: f64,
    },
    /// Emergency stop: halts the current job
    Stop,
    /// Move the head around a circle starting from where it is now
    Circle {
        #[arg(default_value_t = 10.0)]
        radius_mm: f64,
        #[arg(long, default_value_t = 36)]
        segments: u32,
        #[arg(long, default_value_t = 3000.0)]
        feed: f64,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if let Cmd::Discover = cli.command {
        let serial = printer::discover_serial(&cli.ip, &cli.access_code)?;
        println!("Serial number: {serial}");
        return Ok(());
    }

    let Some(serial) = cli.serial.as_deref() else {
        bail!("--serial (or A1_SERIAL) is required for this command; run `discover` to find it");
    };
    let mut p = Printer::connect(&cli.ip, serial, &cli.access_code)?;

    match cli.command {
        Cmd::Discover => unreachable!(),
        Cmd::Status => {
            let s = p.read_status(Duration::from_secs(5))?;
            println!("state:  {}", s.gcode_state.unwrap_or_else(|| "unknown".into()));
            println!("nozzle: {} C", fmt_temp(s.nozzle_temp));
            println!("bed:    {} C", fmt_temp(s.bed_temp));
            if s.fans.is_empty() {
                println!("fans:   not reported");
            }
            for (name, speed) in &s.fans {
                println!("{name}: {speed}");
            }
        }
        Cmd::Home { axes } => {
            let parsed = axes.iter().map(|a| Axis::parse(a)).collect::<Result<Vec<_>>>()?;
            p.send_gcode(&gcode::home(&parsed))?;
        }
        Cmd::Jog { axis, distance_mm, feed } => {
            let lines = gcode::jog(Axis::parse(&axis)?, distance_mm, feed)?;
            p.send_gcode(&lines)?;
        }
        Cmd::Nozzle { celsius } => {
            p.send_gcode(&gcode::set_nozzle_temp(celsius)?)?;
        }
        Cmd::Load { mm, feed } => filament(&mut p, mm, feed)?,
        Cmd::Unload { mm, feed } => filament(&mut p, -mm.abs(), feed)?,
        Cmd::Stop => p.stop()?,
        Cmd::Circle { radius_mm, segments, feed } => {
            p.send_gcode(&a1_control::shapes::circle(radius_mm, segments, feed)?)?;
        }
    }
    Ok(())
}

/// Check the nozzle is hot before moving filament, then send the move.
fn filament(p: &mut Printer, mm: f64, feed: f64) -> Result<()> {
    let s = p.read_status(Duration::from_secs(5))?;
    let Some(temp) = s.nozzle_temp else {
        bail!("could not read nozzle temperature; refusing to move filament");
    };
    p.send_gcode(&gcode::move_filament(mm, feed, temp)?)
}

fn fmt_temp(t: Option<f64>) -> String {
    t.map(|v| format!("{v:.1}")).unwrap_or_else(|| "?".into())
}
