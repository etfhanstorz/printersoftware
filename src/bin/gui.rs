//! Desktop app for the Bambu A1: buttons for status, homing, jogging,
//! nozzle heat and filament. Each action opens its own LAN connection to the
//! printer in a background thread, so the window stays responsive.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use anyhow::{Result, anyhow};
use eframe::egui;

use a1_control::gcode::{self, Axis};
use a1_control::printer::Printer;

type Job = Box<dyn FnOnce(&mut Printer) -> Result<String> + Send>;

struct App {
    ip: String,
    serial: String,
    access_code: String,
    jog_mm: f64,
    feed: f64,
    nozzle_target: f64,
    filament_mm: f64,
    busy: bool,
    log: Vec<String>,
    tx: Sender<Result<String, String>>,
    rx: Receiver<Result<String, String>>,
}

impl App {
    fn new() -> Self {
        let (tx, rx) = channel();
        let saved = load_saved_connection();
        App {
            ip: saved.ip,
            serial: saved.serial,
            access_code: saved.access_code,
            jog_mm: 10.0,
            feed: 3000.0,
            nozzle_target: 200.0,
            filament_mm: 50.0,
            busy: false,
            log: vec!["Ready. Press Status to check the printer.".into()],
            tx,
            rx,
        }
    }

    /// Run one action on a background thread. Only one normal action at a time.
    fn run(&mut self, label: &str, job: Job) {
        if self.busy {
            self.log.push("Busy: wait for the current action to finish.".into());
            return;
        }
        self.busy = true;
        self.log.push(format!("> {label}"));
        self.spawn(job);
    }

    fn spawn(&self, job: Job) {
        let (ip, serial, code) = (self.ip.clone(), self.serial.clone(), self.access_code.clone());
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = (|| -> Result<String> {
                let mut p = Printer::connect(&ip, &serial, &code)?;
                job(&mut p)
            })();
            let _ = tx.send(result.map_err(|e| e.to_string()));
        });
    }

    fn drain_results(&mut self) {
        while let Ok(result) = self.rx.try_recv() {
            self.busy = false;
            match result {
                Ok(msg) => self.log.push(msg),
                Err(e) => self.log.push(format!("Error: {e}")),
            }
        }
    }

    fn status_job() -> Job {
        Box::new(|p| {
            let s = p.read_status(Duration::from_secs(5))?;
            Ok(format!(
                "State: {}  |  Nozzle: {} C  |  Bed: {} C",
                s.gcode_state.unwrap_or_else(|| "unknown".into()),
                fmt(s.nozzle_temp),
                fmt(s.bed_temp)
            ))
        })
    }
}

/// Printer connection details, kept in `printer.local.json` (git-ignored).
#[derive(Default, serde::Deserialize)]
struct SavedConnection {
    #[serde(default)]
    ip: String,
    #[serde(default)]
    serial: String,
    #[serde(default)]
    access_code: String,
}

/// Read `printer.local.json` from the folder the app is started in, if present.
fn load_saved_connection() -> SavedConnection {
    std::fs::read_to_string("printer.local.json")
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn fmt(t: Option<f64>) -> String {
    t.map(|v| format!("{v:.1}")).unwrap_or_else(|| "?".into())
}

impl eframe::App for App {
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_results();
        if self.busy {
            root.ctx().request_repaint_after(Duration::from_millis(200));
        }
        self.handle_keys(root);

        egui::CentralPanel::default().show(root, |ui| {
            ui.heading("A1 Control");
            ui.label("Keys: W/S = Y, A/D = X, R/F = Z, Space = emergency stop");
            ui.add_enabled_ui(!self.busy, |ui| {
                egui::CollapsingHeader::new("Printer connection")
                    .default_open(true)
                    .show(ui, |ui| {
                        egui::Grid::new("conn").num_columns(2).show(ui, |ui| {
                            ui.label("IP");
                            ui.text_edit_singleline(&mut self.ip);
                            ui.end_row();
                            ui.label("Serial");
                            ui.text_edit_singleline(&mut self.serial);
                            ui.end_row();
                            ui.label("Access code");
                            ui.text_edit_singleline(&mut self.access_code);
                            ui.end_row();
                        });
                    });

                ui.separator();
                if ui.button("Check status").clicked() {
                    self.run("Check status", App::status_job());
                }

                ui.separator();
                ui.label("Home");
                ui.horizontal(|ui| {
                    if ui.button("Home all").clicked() {
                        let lines = gcode::home(&[]);
                        self.run("Home all", Box::new(move |p| { p.send_gcode(&lines)?; Ok("Homing sent".into()) }));
                    }
                    for axis in [Axis::X, Axis::Y, Axis::Z] {
                        if ui.button(format!("Home {}", axis.letter())).clicked() {
                            let lines = gcode::home(&[axis]);
                            self.run("Home", Box::new(move |p| { p.send_gcode(&lines)?; Ok("Homing sent".into()) }));
                        }
                    }
                });

                ui.separator();
                ui.label("Jog");
                ui.horizontal(|ui| {
                    ui.label("Step (mm)");
                    ui.add(egui::DragValue::new(&mut self.jog_mm).range(0.1..=100.0).speed(0.5));
                    ui.label("Speed (mm/min)");
                    ui.add(egui::DragValue::new(&mut self.feed).range(1.0..=10_000.0).speed(50.0));
                });
                for axis in [Axis::X, Axis::Y, Axis::Z] {
                    ui.horizontal(|ui| {
                        ui.label(axis.letter().to_string());
                        for sign in [-1.0, 1.0] {
                            let dist = sign * self.jog_mm;
                            let label = if sign < 0.0 { format!("-{}", self.jog_mm) } else { format!("+{}", self.jog_mm) };
                            if ui.button(label).clicked() {
                                match gcode::jog(axis, dist, self.feed) {
                                    Ok(lines) => self.run(
                                        &format!("Jog {} {dist}", axis.letter()),
                                        Box::new(move |p| { p.send_gcode(&lines)?; Ok("Move sent".into()) }),
                                    ),
                                    Err(e) => self.log.push(format!("Error: {e}")),
                                }
                            }
                        }
                    });
                }

                ui.separator();
                ui.label("Nozzle");
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut self.nozzle_target).range(0.0..=300.0).suffix(" C"));
                    if ui.button("Heat nozzle").clicked() {
                        match gcode::set_nozzle_temp(self.nozzle_target) {
                            Ok(lines) => self.run("Heat nozzle", Box::new(move |p| { p.send_gcode(&lines)?; Ok("Heating".into()) })),
                            Err(e) => self.log.push(format!("Error: {e}")),
                        }
                    }
                });

                ui.separator();
                ui.label("Filament (needs nozzle at 170 C or more)");
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut self.filament_mm).range(1.0..=200.0).suffix(" mm"));
                    if ui.button("Load").clicked() {
                        self.filament_job(self.filament_mm);
                    }
                    if ui.button("Unload").clicked() {
                        self.filament_job(-self.filament_mm);
                    }
                });
            });

            ui.separator();
            if ui
                .add(egui::Button::new("EMERGENCY STOP").fill(egui::Color32::from_rgb(170, 30, 30)))
                .clicked()
            {
                // Stop is sent even while another action is running.
                self.log.push("> EMERGENCY STOP".into());
                self.spawn(Box::new(|p| { p.stop()?; Ok("Stop sent".into()) }));
            }

            ui.separator();
            ui.label("Log");
            egui::ScrollArea::vertical().stick_to_bottom(true).max_height(220.0).show(ui, |ui| {
                for line in &self.log {
                    ui.monospace(line);
                }
            });
        });
    }
}

impl App {
    /// WASD (plus R/F for Z) jogs one step per key press; Space is emergency stop.
    /// Ignored while typing in a text box.
    fn handle_keys(&mut self, root: &egui::Ui) {
        if root.ctx().egui_wants_keyboard_input() {
            return;
        }
        let (jogs, stop) = root.input(|i| {
            let mut jogs = Vec::new();
            for (key, axis, sign) in [
                (egui::Key::A, Axis::X, -1.0),
                (egui::Key::D, Axis::X, 1.0),
                (egui::Key::S, Axis::Y, -1.0),
                (egui::Key::W, Axis::Y, 1.0),
                (egui::Key::F, Axis::Z, -1.0),
                (egui::Key::R, Axis::Z, 1.0),
            ] {
                if i.key_pressed(key) {
                    jogs.push((axis, sign));
                }
            }
            (jogs, i.key_pressed(egui::Key::Space))
        });

        if stop {
            self.log.push("> EMERGENCY STOP (Space)".into());
            self.spawn(Box::new(|p| { p.stop()?; Ok("Stop sent".into()) }));
        }
        for (axis, sign) in jogs {
            let dist = sign * self.jog_mm;
            match gcode::jog(axis, dist, self.feed) {
                Ok(lines) => self.run(
                    &format!("Jog {} {dist}", axis.letter()),
                    Box::new(move |p| { p.send_gcode(&lines)?; Ok("Move sent".into()) }),
                ),
                Err(e) => self.log.push(format!("Error: {e}")),
            }
        }
    }

    fn filament_job(&mut self, mm: f64) {
        let feed = 300.0;
        self.run(
            if mm < 0.0 { "Unload filament" } else { "Load filament" },
            Box::new(move |p| {
                let s = p.read_status(Duration::from_secs(5))?;
                let temp = s
                    .nozzle_temp
                    .ok_or_else(|| anyhow!("could not read nozzle temperature; not moving filament"))?;
                p.send_gcode(&gcode::move_filament(mm, feed, temp)?)?;
                Ok(format!("Filament moved {mm} mm"))
            }),
        );
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([520.0, 720.0]),
        ..Default::default()
    };
    eframe::run_native(
        "A1 Control",
        options,
        Box::new(|_cc| Ok(Box::new(App::new()))),
    )
}
