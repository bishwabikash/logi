//! egui front end. All device work goes through `device::Handle`, which talks
//! to a worker thread, so nothing here can block the frame loop.

use crate::device::{self, Cmd, Snapshot};
use crate::dpi::Capability;
use crate::led;
use crate::profile_blob::Led;
use eframe::egui;

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Sensor,
    Lighting,
    Profiles,
    Buttons,
    About,
}

pub struct App {
    dev: device::Handle,
    tab: Tab,
    snap: Snapshot,

    // Sensor
    dpi: u16,

    // Lighting
    zone_sel: usize,
    all_zones: bool,
    mode: u8,
    color: [f32; 3],
    period_ms: u16,
    waveform: usize,
    intensity: u8,

    // Effects
    effect: usize,
    effect_period_ms: u32,

    profile_sel: usize,
    backup_path: String,
}

impl App {
    pub fn new() -> Self {
        App {
            dev: device::spawn(),
            tab: Tab::Sensor,
            snap: Snapshot::default(),
            dpi: 1600,
            zone_sel: 0,
            all_zones: true,
            mode: led::FIXED,
            color: [0.0, 1.0, 1.0],
            period_ms: 4000,
            waveform: 0,
            intensity: 100,
            effect: 0,
            effect_period_ms: 4000,
            profile_sel: 0,
            backup_path: default_backup_path(),
        }
    }

    fn rgb(&self) -> [u8; 3] {
        [
            (self.color[0] * 255.0).round() as u8,
            (self.color[1] * 255.0).round() as u8,
            (self.color[2] * 255.0).round() as u8,
        ]
    }

    fn zones(&self) -> Vec<u8> {
        if self.all_zones {
            self.snap.zones.iter().map(|z| z.index).collect()
        } else {
            self.snap
                .zones
                .get(self.zone_sel)
                .map(|z| vec![z.index])
                .unwrap_or_default()
        }
    }
}

fn default_backup_path() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    format!("{home}/g502-backup.json")
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.snap = self.dev.snapshot();
        // The worker polls for the device; keep repainting so a replug shows up.
        ctx.request_repaint_after(std::time::Duration::from_millis(500));

        egui::TopBottomPanel::top("head").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading("G502");
                ui.add_space(8.0);
                if self.snap.connected {
                    ui.label(
                        egui::RichText::new(&self.snap.model).color(egui::Color32::LIGHT_GREEN),
                    );
                    ui.label(
                        egui::RichText::new(format!("HID++ {}", self.snap.protocol)).weak(),
                    );
                    let mode = crate::profiles::mode_name(self.snap.mode);
                    ui.label(egui::RichText::new(format!("mode: {mode}")).weak());
                } else {
                    ui.label(
                        egui::RichText::new("no device").color(egui::Color32::LIGHT_RED),
                    );
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Refresh").clicked() {
                        self.dev.send(Cmd::Refresh);
                    }
                });
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                for (t, label) in [
                    (Tab::Sensor, "Sensor"),
                    (Tab::Lighting, "Lighting"),
                    (Tab::Profiles, "Profiles"),
                    (Tab::Buttons, "Buttons"),
                    (Tab::About, "About"),
                ] {
                    if ui.selectable_label(self.tab == t, label).clicked() {
                        self.tab = t;
                    }
                }
            });
            ui.add_space(4.0);
        });

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.add_space(4.0);
            if let Some(e) = &self.snap.error {
                ui.label(egui::RichText::new(e).color(egui::Color32::LIGHT_RED));
            } else if let Some(n) = &self.snap.notice {
                ui.label(egui::RichText::new(n).color(egui::Color32::LIGHT_BLUE));
            } else {
                ui.label(egui::RichText::new("ready").weak());
            }
            ui.add_space(4.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            if !self.snap.connected {
                self.draw_disconnected(ui);
                return;
            }
            egui::ScrollArea::vertical().show(ui, |ui| match self.tab {
                Tab::Sensor => self.draw_sensor(ui),
                Tab::Lighting => self.draw_lighting(ui),
                Tab::Profiles => self.draw_profiles(ui),
                Tab::Buttons => self.draw_buttons(ui),
                Tab::About => self.draw_about(ui),
            });
        });
    }
}

impl App {
    fn draw_disconnected(&mut self, ui: &mut egui::Ui) {
        ui.add_space(30.0);
        ui.vertical_centered(|ui| {
            ui.heading("No G502 found");
            ui.add_space(8.0);
            let err = self.snap.error.clone().unwrap_or_default();
            if err.contains("permission") {
                ui.label("The mouse is attached but this user cannot open its hidraw node.");
                ui.add_space(8.0);
                ui.label("Install the udev rule, then unplug and replug the mouse:");
                ui.add_space(4.0);
                ui.code("packaging/install.sh");
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new(
                        "udevadm trigger re-runs the rules, but a node that is already \
                         open keeps its old permissions, so the replug matters.",
                    )
                    .weak(),
                );
            } else {
                ui.label("Plug in a Logitech G502 over USB.");
                if !err.is_empty() {
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new(err).weak());
                }
            }
        });
    }

    fn draw_sensor(&mut self, ui: &mut egui::Ui) {
        ui.heading("Sensor");
        ui.add_space(6.0);

        let (min, max, step) = match self.snap.dpi_cap {
            Some(Capability::Range { min, max, step }) => (min, max, step),
            _ => (100, 25600, 50),
        };
        if self.dpi == 1600 && self.snap.dpi != 0 {
            self.dpi = self.snap.dpi;
        }
        ui.label(format!("Current: {} DPI", self.snap.dpi));
        ui.add_space(4.0);
        let slider = egui::Slider::new(&mut self.dpi, min..=max)
            .step_by(step as f64)
            .text("DPI");
        if ui.add(slider).drag_stopped() {
            self.dev.send(Cmd::SetDpi(self.dpi));
        }
        ui.horizontal(|ui| {
            for preset in [400u16, 800, 1600, 3200, 6400, 12800] {
                if preset <= max && ui.button(preset.to_string()).clicked() {
                    self.dpi = preset;
                    self.dev.send(Cmd::SetDpi(preset));
                }
            }
        });
        ui.label(
            egui::RichText::new(format!(
                "This sensor is continuous: {min}-{max} in steps of {step}, not fixed stages."
            ))
            .weak(),
        );

        ui.add_space(14.0);
        ui.heading("Report rate");
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            for hz in self.snap.rates.clone() {
                let on = self.snap.rate == hz;
                if ui.selectable_label(on, format!("{hz} Hz")).clicked() {
                    self.dev.send(Cmd::SetRate(hz));
                }
            }
        });
    }

    fn draw_lighting(&mut self, ui: &mut egui::Ui) {
        ui.heading("Lighting");
        ui.add_space(6.0);

        ui.horizontal(|ui| {
            ui.checkbox(&mut self.all_zones, "Both zones");
            if !self.all_zones {
                for (i, z) in self.snap.zones.clone().iter().enumerate() {
                    let on = self.zone_sel == i;
                    if ui.selectable_label(on, format!("{} ({})", z.index, z.location)).clicked() {
                        self.zone_sel = i;
                    }
                }
            }
        });

        // Only offer modes the firmware actually implements.
        let available: Vec<u8> = self
            .snap
            .zones
            .first()
            .map(|z| z.modes.clone())
            .unwrap_or_else(|| vec![led::OFF, led::FIXED, led::BREATHING, led::CYCLE]);

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label("Mode:");
            for m in &available {
                if ui.selectable_label(self.mode == *m, led::mode_name(*m)).clicked() {
                    self.mode = *m;
                }
            }
        });

        ui.add_space(8.0);
        if self.mode == led::FIXED || self.mode == led::BREATHING {
            ui.horizontal(|ui| {
                ui.label("Colour:");
                ui.color_edit_button_rgb(&mut self.color);
            });
        }
        if self.mode == led::BREATHING || self.mode == led::CYCLE {
            ui.add(egui::Slider::new(&mut self.period_ms, 500..=20000).text("period (ms)"));
            ui.add(egui::Slider::new(&mut self.intensity, 1..=100).text("intensity (%)"));
        }
        if self.mode == led::BREATHING {
            ui.horizontal(|ui| {
                ui.label("Waveform:");
                egui::ComboBox::from_id_salt("wf")
                    .selected_text(led::WAVEFORMS[self.waveform].0)
                    .show_ui(ui, |ui| {
                        for (i, (n, _)) in led::WAVEFORMS.iter().enumerate() {
                            ui.selectable_value(&mut self.waveform, i, *n);
                        }
                    });
            });
        }

        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if ui.button("Apply (live)").clicked() {
                let c = self.rgb();
                let wf = led::WAVEFORMS[self.waveform].1;
                for z in self.zones() {
                    self.dev.send(match self.mode {
                        led::OFF => Cmd::LedOff { zone: z },
                        led::BREATHING => Cmd::LedBreathing {
                            zone: z, color: c, period: self.period_ms,
                            waveform: wf, intensity: self.intensity,
                        },
                        led::CYCLE => Cmd::LedCycle {
                            zone: z, period: self.period_ms, intensity: self.intensity,
                        },
                        _ => Cmd::LedFixed { zone: z, color: c },
                    });
                }
            }
            if ui.button("Store to onboard profile").clicked() {
                let c = self.rgb();
                let wf = led::WAVEFORMS[self.waveform].1;
                let rec = match self.mode {
                    led::OFF => Led::off(),
                    led::BREATHING => Led::breathing(c, self.period_ms, wf, self.intensity),
                    led::CYCLE => Led::cycle(self.period_ms, self.intensity),
                    _ => Led::fixed(c),
                };
                self.dev.send(Cmd::PersistLighting { leds: [rec, rec] });
            }
        });
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new(
                "Apply (live) needs host mode and is lost when the mouse is unplugged. \
                 Store writes it into onboard flash, so it persists without this app \
                 and into Windows.",
            )
            .weak(),
        );

        ui.add_space(18.0);
        ui.heading("Effects");
        ui.label(
            egui::RichText::new(
                "Animation is streamed from this app, because the firmware has no \
                 frame mode. It stops when the effect stops.",
            )
            .weak(),
        );
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("fx")
                .selected_text(crate::effects::EFFECTS[self.effect].0)
                .show_ui(ui, |ui| {
                    for (i, (n, _)) in crate::effects::EFFECTS.iter().enumerate() {
                        ui.selectable_value(&mut self.effect, i, *n);
                    }
                });
            ui.label(egui::RichText::new(crate::effects::EFFECTS[self.effect].1).weak());
        });
        ui.add(egui::Slider::new(&mut self.effect_period_ms, 250..=20000).text("period (ms)"));
        ui.horizontal(|ui| {
            if ui.button("Start").clicked() {
                self.dev.send(Cmd::StartEffect {
                    name: crate::effects::EFFECTS[self.effect].0.to_string(),
                    color: self.rgb(),
                    period_ms: self.effect_period_ms,
                });
            }
            if ui.button("Stop").clicked() {
                self.dev.send(Cmd::StopEffect);
            }
            if let Some(r) = &self.snap.effect_running {
                ui.label(egui::RichText::new(format!("running: {r}")).color(egui::Color32::LIGHT_GREEN));
            }
        });
    }

    fn draw_profiles(&mut self, ui: &mut egui::Ui) {
        ui.heading("Onboard profiles");
        ui.label(
            egui::RichText::new(format!(
                "{} profiles stored on the mouse. The active one applies with no software running.",
                self.snap.profile_count
            ))
            .weak(),
        );
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            for i in 0..self.snap.profile_count {
                let on = self.snap.active_profile == i;
                if ui.selectable_label(on, format!("Profile {i}")).clicked() {
                    self.dev.send(Cmd::SetProfile(i));
                }
            }
        });

        ui.add_space(12.0);
        if !self.snap.profiles.is_empty() {
            self.profile_sel = self.profile_sel.min(self.snap.profiles.len() - 1);
            ui.horizontal(|ui| {
                ui.label("Inspect:");
                for i in 0..self.snap.profiles.len() {
                    if ui.selectable_label(self.profile_sel == i, i.to_string()).clicked() {
                        self.profile_sel = i;
                    }
                }
            });
            ui.add_space(6.0);
            let n = self.snap.button_count as usize;
            let text = self.snap.profiles[self.profile_sel].describe(n);
            ui.group(|ui| {
                ui.add(egui::Label::new(egui::RichText::new(text).monospace()).wrap());
            });
        }

        ui.add_space(14.0);
        ui.heading("Backup");
        ui.label(
            egui::RichText::new(
                "Dumps all 16 sectors, including the profile directory. Take one before \
                 writing anything to flash.",
            )
            .weak(),
        );
        ui.horizontal(|ui| {
            ui.text_edit_singleline(&mut self.backup_path);
            if ui.button("Save backup").clicked() {
                self.dev.send(Cmd::Backup(self.backup_path.clone()));
            }
        });
    }

    fn draw_buttons(&mut self, ui: &mut egui::Ui) {
        ui.heading("Buttons");
        if self.snap.profiles.is_empty() {
            ui.label("No profile read yet.");
            return;
        }
        let idx = self.snap.active_profile as usize;
        let Some(p) = self.snap.profiles.get(idx) else {
            ui.label("Active profile not readable.");
            return;
        };
        ui.label(
            egui::RichText::new(format!(
                "Profile {idx}, {} buttons, decoded from onboard flash.",
                self.snap.button_count
            ))
            .weak(),
        );
        ui.add_space(8.0);
        egui::Grid::new("btns").striped(true).show(ui, |ui| {
            ui.label(egui::RichText::new("#").strong());
            ui.label(egui::RichText::new("bound to").strong());
            ui.label(egui::RichText::new("raw").strong());
            ui.end_row();
            for (i, b) in p.buttons.iter().take(self.snap.button_count as usize).enumerate() {
                ui.label(i.to_string());
                ui.label(b.describe());
                ui.label(
                    egui::RichText::new(format!(
                        "{:02x} {:02x} {:02x} {:02x}",
                        b.0[0], b.0[1], b.0[2], b.0[3]
                    ))
                    .monospace()
                    .weak(),
                );
                ui.end_row();
            }
        });
        ui.add_space(10.0);
        ui.label(
            egui::RichText::new(
                "Rebinding writes to onboard flash. Not wired into this view yet; \
                 use the CLI, and take a backup first.",
            )
            .weak(),
        );
    }

    fn draw_about(&mut self, ui: &mut egui::Ui) {
        ui.heading("About");
        ui.add_space(6.0);
        ui.label(format!("Device : {}", self.snap.model));
        ui.label(format!("Node   : {}", self.snap.path));
        ui.label(format!("HID++  : {}", self.snap.protocol));
        ui.add_space(10.0);
        ui.label("What this hardware can and cannot do:");
        ui.add_space(4.0);
        for line in [
            "The firmware implements four lighting modes: off, fixed, breathing, cycle.",
            "Wave, starlight, ripple and frame-streaming are defined by HID++ but absent here.",
            "So animated effects are streamed by this app and stop when it stops.",
            "DPI is continuous, 100-25600 in steps of 50.",
            "Surface tuning and lift-off distance are not implemented: they sit behind",
            "engineering-flagged features that can brick the mouse, so they are out of scope.",
        ] {
            ui.label(egui::RichText::new(line).weak());
        }
        ui.add_space(10.0);
        ui.label(egui::RichText::new("Protocol detail is in PROTOCOL.md.").weak());
    }
}

pub fn run() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([760.0, 620.0])
            .with_min_inner_size([560.0, 420.0])
            .with_title("G502"),
        ..Default::default()
    };
    eframe::run_native("g502", options, Box::new(|_cc| Ok(Box::new(App::new()))))
}
