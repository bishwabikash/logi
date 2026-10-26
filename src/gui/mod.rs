//! egui front end. All device work goes through `device::Handle`, which talks
//! to a worker thread, so nothing here can block the frame loop.

use crate::device::{self, Cmd, Snapshot};
use crate::dpi::Capability;
use crate::led;
use crate::profile_blob::{Button, Led, SPECIALS};
use eframe::egui;

pub mod bindings_editor;
pub mod profile_editor;
pub mod rules_editor;
pub mod theme;

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Sensor,
    Lighting,
    Profiles,
    Buttons,
    Apps,
    Service,
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

    /// Pending button edits, keyed by button index, applied on click.
    btn_action: std::collections::HashMap<u8, String>,

    themed: bool,
    profile_editor: profile_editor::ProfileEditor,
    rules: rules_editor::RulesEditor,
    bindings: bindings_editor::BindingsEditor,
    service: crate::service::Watcher,
    /// Editors read their config files once, off the first paint.
    loaded: bool,
    /// None when there is no StatusNotifierWatcher, which is normal on a
    /// desktop without a tray host. The app works either way.
    tray: Option<crate::tray::Tray>,
    tray_tried: bool,
    /// True while the DPI slider is being dragged, so the live readout is not
    /// overwritten by the device's own value mid-drag.
    dpi_touched: bool,
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
            btn_action: std::collections::HashMap::new(),
            themed: false,
            profile_editor: profile_editor::ProfileEditor::new(),
            rules: rules_editor::RulesEditor::new(),
            bindings: bindings_editor::BindingsEditor::new(),
            service: crate::service::Watcher::new(),
            loaded: false,
            tray: None,
            tray_tried: false,
            dpi_touched: false,
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

/// Map a menu label back to a binding. Labels come from the same tables that
/// produced them, so an unknown one means a bug rather than user input.
fn action_to_button(label: &str) -> Option<Button> {
    if label == "disabled" {
        return Some(Button::disabled());
    }
    if let Some(rest) = label.strip_prefix("button ") {
        return rest.trim().parse::<u8>().ok().map(Button::mouse);
    }
    Button::special(label)
}

fn default_backup_path() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    format!("{home}/logi-backup.json")
}

impl eframe::App for App {
    /// Hand the mouse back when the window closes.
    ///
    /// Host mode does not affect buttons, but it does stop the stored profile
    /// being applied, so a mouse left in it keeps whatever lighting this app
    /// last pushed and ignores its own profile. Restoring on exit means
    /// closing the app always returns the mouse to driving itself.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.dev.restore_onboard_blocking();
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.themed {
            theme::apply(ctx);
            self.themed = true;
        }
        if !self.loaded {
            // File IO, so once rather than every frame.
            self.rules.load();
            self.bindings.load();
            self.loaded = true;
        }
        self.sync_tray(ctx);
        self.snap = self.dev.snapshot();
        // The worker polls for the device, so keep repainting to pick up a
        // replug without the user having to click anything.
        ctx.request_repaint_after(std::time::Duration::from_millis(500));

        self.draw_sidebar(ctx);
        self.draw_status(ctx);

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::BG).inner_margin(egui::Margin::symmetric(22, 18)))
            .show(ctx, |ui| {
                if !self.snap.connected {
                    self.draw_disconnected(ui);
                    return;
                }
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    // Cap the reading width, but never beyond what the panel
                    // actually has: forcing 640 here clipped every card.
                    let w = ui.available_width().min(660.0);
                    ui.set_max_width(w);
                    match self.tab {
                        Tab::Sensor => self.draw_sensor(ui),
                        Tab::Lighting => self.draw_lighting(ui),
                        Tab::Profiles => self.draw_profiles(ui),
                        Tab::Buttons => self.draw_buttons(ui),
                        Tab::Apps => {
                            let snap = self.snap.clone();
                            self.rules.ui(ui, &snap);
                        }
                        Tab::Service => self.draw_service(ui),
                        Tab::About => self.draw_about(ui),
                    }
                });
            });
    }
}

const PALETTE: &[[u8; 3]] = &[
    [0x2d, 0xd4, 0xd4], [0xff, 0xff, 0xff], [0xe8, 0x3b, 0x3b], [0xff, 0x7a, 0x1f],
    [0xff, 0xd4, 0x2a], [0x5a, 0xd1, 0x5a], [0x3b, 0x8c, 0xff], [0x9b, 0x5a, 0xff],
    [0xff, 0x4d, 0x9e],
];

impl App {
    /// Register the tray once, then keep it in step and act on its menu.
    fn sync_tray(&mut self, ctx: &egui::Context) {
        use crate::tray::{Event, Status};

        // Build the real status first: registering with Status::default()
        // made the first icon claim a service state nothing had looked at yet.
        let status = Status {
            device: self.snap.connected.then(|| self.snap.model.clone()),
            service: self.service.state(),
            active_profile: self.snap.active_profile,
            profile_count: self.snap.profile_count,
            dpi: self.snap.dpi,
            // The device's own range, not a fixed list: with no mouse attached
            // there is nothing to offer, and offering it anyway produced a DPI
            // menu that could not work.
            dpi_presets: match (self.snap.connected, &self.snap.dpi_cap) {
                (true, Some(Capability::Range { min, max, .. })) => {
                    [400u16, 800, 1600, 3200, 6400]
                        .into_iter()
                        .filter(|p| p >= min && p <= max)
                        .collect()
                }
                (true, Some(Capability::List(v))) => v.clone(),
                _ => Vec::new(),
            },
        };

        if !self.tray_tried {
            self.tray_tried = true;
            match crate::tray::Tray::spawn(status.clone()) {
                Ok(t) => self.tray = Some(t),
                Err(e) => eprintln!("logi: no tray icon: {e}"),
            }
        }
        let Some(tray) = &self.tray else { return };

        // update() is a no-op when nothing changed, so this is cheap per frame.
        tray.update(status);

        for ev in tray.poll() {
            match ev {
                Event::Open => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                }
                Event::ToggleService => {
                    let a = if self.service.state() == crate::service::State::Active {
                        crate::service::Action::Stop
                    } else {
                        crate::service::Action::Start
                    };
                    self.service.request(a);
                }
                Event::RestartService => self.service.request(crate::service::Action::Restart),
                Event::SetProfile(p) => {
                    self.dev.send(Cmd::SetProfile(p));
                }
                Event::SetDpi(d) => {
                    self.dev.send(Cmd::SetDpi(d));
                }
                Event::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            }
        }
    }

    fn draw_sidebar(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("nav")
            .exact_width(186.0)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(theme::RAISED)
                    .inner_margin(egui::Margin::symmetric(14, 16)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Logi").size(21.0).strong().color(theme::TEXT));
                });
                ui.add_space(2.0);
                if self.snap.connected {
                    ui.label(
                        egui::RichText::new(self.snap.model.replace(" Gaming Mouse", ""))
                            .size(11.5)
                            .color(theme::DIM),
                    );
                    ui.add_space(9.0);
                    ui.horizontal_wrapped(|ui| {
                        theme::pill(ui, "connected", theme::GOOD);
                        let m = crate::profiles::mode_name(self.snap.mode);
                        let c = if self.snap.mode == crate::profiles::MODE_HOST {
                            theme::WARN
                        } else {
                            theme::DIM
                        };
                        theme::pill(ui, m, c);
                    });
                } else {
                    ui.add_space(9.0);
                    theme::pill(ui, "no device", theme::BAD);
                }

                ui.add_space(18.0);
                for (t, label) in [
                    (Tab::Sensor, "Sensor"),
                    (Tab::Lighting, "Lighting"),
                    (Tab::Profiles, "Profiles"),
                    (Tab::Buttons, "Buttons"),
                    (Tab::Apps, "Apps"),
                    (Tab::Service, "Service"),
                    (Tab::About, "About"),
                ] {
                    let on = self.tab == t;
                    let txt = egui::RichText::new(label)
                        .size(14.5)
                        .color(if on { theme::TEXT } else { theme::DIM });
                    let resp = ui.add_sized(
                        egui::vec2(ui.available_width(), 32.0),
                        egui::SelectableLabel::new(on, txt),
                    );
                    if resp.clicked() {
                        self.tab = t;
                    }
                }

                ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                    if ui.button("Refresh").clicked() {
                        self.dev.send(Cmd::Refresh);
                    }
                    ui.add_space(14.0);

                    // Memory mode. Host does not affect buttons or movement
                    // (measured: input is identical in both modes). What it
                    // does change is that the stored profile is not applied,
                    // so lighting is whatever this app last pushed.
                    let host = self.snap.mode == crate::profiles::MODE_HOST;
                    theme::hint(
                        ui,
                        if host {
                            "Host: this app drives the lighting. The stored profile is \
                             not applied, so lighting reverts when the app exits. \
                             Buttons and movement are unaffected."
                        } else {
                            "Onboard: the mouse drives itself from its stored profile, \
                             with nothing running."
                        },
                    );
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.selectable_label(!host, "Onboard").clicked() && host {
                            self.dev.send(Cmd::SetMode(crate::profiles::MODE_ONBOARD));
                        }
                        if ui.selectable_label(host, "Host").clicked() && !host {
                            self.dev.send(Cmd::SetMode(crate::profiles::MODE_HOST));
                        }
                    });
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new("MEMORY MODE")
                            .color(theme::DIM)
                            .size(11.0)
                            .strong(),
                    );
                });
            });
    }

    fn draw_status(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("status")
            .frame(
                egui::Frame::new()
                    .fill(theme::RAISED)
                    .inner_margin(egui::Margin::symmetric(16, 8)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if let Some(e) = &self.snap.error {
                        ui.label(egui::RichText::new("!").color(theme::BAD).strong());
                        ui.label(egui::RichText::new(e).color(theme::BAD).size(12.5));
                    } else if let Some(n) = &self.snap.notice {
                        ui.label(egui::RichText::new("OK").color(theme::GOOD).size(12.0).strong());
                        ui.label(egui::RichText::new(n).size(12.5));
                    } else {
                        ui.label(egui::RichText::new("Ready").color(theme::DIM).size(12.5));
                    }
                    if let Some(r) = &self.snap.effect_running {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            theme::pill(ui, &format!("{r} running"), theme::ACCENT);
                        });
                    }
                });
            });
    }

    fn draw_disconnected(&mut self, ui: &mut egui::Ui) {
        ui.add_space(40.0);
        let err = self.snap.error.clone().unwrap_or_default();
        theme::card(ui, |ui| {
            ui.label(egui::RichText::new("No G502 found").size(18.0).strong());
            ui.add_space(10.0);
            if err.contains("permission") {
                theme::hint(
                    ui,
                    "The mouse is attached, but this user cannot open its hidraw node.",
                );
                ui.add_space(10.0);
                ui.label("Install the udev rule, then unplug and replug the mouse:");
                ui.add_space(6.0);
                ui.code("packaging/install.sh");
                ui.add_space(10.0);
                theme::hint(
                    ui,
                    "udevadm trigger re-runs the rules, but a node that is already open \
                     keeps its old permissions, so the replug is what matters.",
                );
            } else {
                theme::hint(ui, "Plug in a Logitech G502 over USB.");
                if !err.is_empty() {
                    ui.add_space(8.0);
                    theme::hint(ui, &err);
                }
            }
        });
    }

    fn draw_sensor(&mut self, ui: &mut egui::Ui) {
        if !self.snap.has_dpi && !self.snap.has_rate {
            theme::unsupported(ui, "an adjustable sensor");
            return;
        }
        let (min, max, step) = match self.snap.dpi_cap {
            Some(Capability::Range { min, max, step }) => (min, max, step),
            _ => (100, 25600, 50),
        };
        if !self.dpi_touched && self.snap.dpi != 0 {
            self.dpi = self.snap.dpi;
        }

        if !self.snap.has_dpi {
            theme::section(ui, "Sensor", |ui| theme::unsupported(ui, "adjustable DPI"));
        } else {
        theme::section(ui, "Sensor", |ui| {
            theme::readout(ui, &self.snap.dpi.to_string(), "DPI");
            ui.add_space(10.0);
            let resp = ui.add(
                egui::Slider::new(&mut self.dpi, min..=max)
                    .step_by(step as f64)
                    .show_value(false),
            );
            if resp.changed() {
                self.dpi_touched = true;
            }
            if resp.drag_stopped() || (resp.changed() && !resp.dragged()) {
                self.dev.send(Cmd::SetDpi(self.dpi));
                self.dpi_touched = false;
            }
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                for p in [400u16, 800, 1600, 3200, 6400, 12800, 25600] {
                    if p < min || p > max {
                        continue;
                    }
                    if ui.selectable_label(self.snap.dpi == p, format!("{p}")).clicked() {
                        self.dpi = p;
                        self.dpi_touched = false;
                        self.dev.send(Cmd::SetDpi(p));
                    }
                }
            });
            ui.add_space(8.0);
            theme::hint(
                ui,
                &format!("Continuous: {min}-{max} in steps of {step}. Not fixed stages."),
            );
        });
        }

        if !self.snap.has_rate {
            theme::section(ui, "Report rate", |ui| {
                theme::unsupported(ui, "an adjustable report rate")
            });
            return;
        }
        theme::section(ui, "Report rate", |ui| {
            theme::value(ui, &self.snap.rate.to_string(), "Hz");
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                for hz in self.snap.rates.clone() {
                    if ui
                        .selectable_label(self.snap.rate == hz, format!("{hz} Hz"))
                        .clicked()
                    {
                        self.dev.send(Cmd::SetRate(hz));
                    }
                }
            });
            ui.add_space(8.0);
            theme::hint(
                ui,
                "Written into the active onboard profile, so it survives unplugging. \
                 The live setter is rejected in onboard mode.",
            );
        });
    }

    fn draw_lighting(&mut self, ui: &mut egui::Ui) {
        if !self.snap.has_leds {
            theme::unsupported(ui, "RGB lighting");
            return;
        }
        let zones = self.snap.zones.clone();
        let live_ok = |m: u8| {
            zones.first().map(|z| z.live_modes.contains(&m)).unwrap_or(false) || m == led::OFF
        };

        theme::section(ui, "Zone", |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui.selectable_label(self.all_zones, "Both").clicked() {
                    self.all_zones = true;
                }
                for (i, z) in zones.iter().enumerate() {
                    let on = !self.all_zones && self.zone_sel == i;
                    if ui.selectable_label(on, &z.location).clicked() {
                        self.all_zones = false;
                        self.zone_sel = i;
                    }
                }
            });
        });

        ui.add_space(14.0);
        theme::section(ui, "Mode", |ui| {
            let available: Vec<u8> = zones
                .first()
                .map(|z| z.modes.clone())
                .unwrap_or_else(|| vec![led::OFF, led::FIXED, led::BREATHING, led::CYCLE]);
            ui.horizontal_wrapped(|ui| {
                for m in &available {
                    let label = if live_ok(*m) {
                        led::mode_name(*m).to_string()
                    } else {
                        format!("{} (store only)", led::mode_name(*m))
                    };
                    if ui.selectable_label(self.mode == *m, label).clicked() {
                        self.mode = *m;
                    }
                }
            });

            if self.mode == led::FIXED || self.mode == led::BREATHING {
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    for c in PALETTE {
                        let sel = self.rgb() == *c;
                        if theme::swatch(ui, *c, sel).clicked() {
                            self.color = [
                                c[0] as f32 / 255.0,
                                c[1] as f32 / 255.0,
                                c[2] as f32 / 255.0,
                            ];
                        }
                    }
                    ui.add_space(6.0);
                    ui.color_edit_button_rgb(&mut self.color);
                });
            }
            if self.mode == led::BREATHING || self.mode == led::CYCLE {
                ui.add_space(10.0);
                ui.add(
                    egui::Slider::new(&mut self.period_ms, 500..=20000)
                        .suffix(" ms")
                        .text("period"),
                );
                ui.add(
                    egui::Slider::new(&mut self.intensity, 1..=100)
                        .suffix(" %")
                        .text("intensity"),
                );
            }
            if self.mode == led::BREATHING {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("waveform").color(theme::DIM).size(12.5));
                    egui::ComboBox::from_id_salt("wf")
                        .selected_text(led::WAVEFORMS[self.waveform].0)
                        .show_ui(ui, |ui| {
                            for (i, (n, _)) in led::WAVEFORMS.iter().enumerate() {
                                ui.selectable_value(&mut self.waveform, i, *n);
                            }
                        });
                });
            }

            ui.add_space(14.0);
            if !live_ok(self.mode) {
                theme::hint(
                    ui,
                    "This firmware will not set cycling live. Store it so the mouse runs \
                     it itself, or stream the rainbow effect below.",
                );
                ui.add_space(8.0);
            }
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(live_ok(self.mode), egui::Button::new("Apply now"))
                    .clicked()
                {
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
                if ui.button("Save to mouse").clicked() {
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
            ui.add_space(8.0);
            theme::hint(
                ui,
                "Apply now needs host mode and is lost on unplug. Save to mouse writes \
                 onboard flash, so it persists with nothing running, and into Windows.",
            );
        });

        ui.add_space(14.0);
        theme::section(ui, "Effects", |ui| {
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("fx")
                    .selected_text(crate::effects::EFFECTS[self.effect].0)
                    .show_ui(ui, |ui| {
                        for (i, (n, _)) in crate::effects::EFFECTS.iter().enumerate() {
                            ui.selectable_value(&mut self.effect, i, *n);
                        }
                    });
                if ui.button("Start").clicked() {
                    self.dev.send(Cmd::StartEffect {
                        name: crate::effects::EFFECTS[self.effect].0.to_string(),
                        color: self.rgb(),
                        period_ms: self.effect_period_ms,
                    });
                }
                if ui
                    .add_enabled(
                        self.snap.effect_running.is_some(),
                        egui::Button::new("Stop"),
                    )
                    .clicked()
                {
                    self.dev.send(Cmd::StopEffect);
                }
            });
            ui.add_space(6.0);
            theme::hint(ui, crate::effects::EFFECTS[self.effect].1);
            ui.add_space(8.0);
            ui.add(
                egui::Slider::new(&mut self.effect_period_ms, 250..=20000)
                    .suffix(" ms")
                    .text("period"),
            );
            ui.add_space(6.0);
            theme::hint(
                ui,
                "The firmware has no frame mode, so animation is streamed from here and \
                 stops when it stops.",
            );
        });
    }

    fn draw_profiles(&mut self, ui: &mut egui::Ui) {
        if !self.snap.has_profiles {
            theme::unsupported(ui, "onboard profiles");
            return;
        }
        if self.snap.profile_format != 2 && self.snap.profiles.is_empty() {
            theme::section(ui, "Active profile", |ui| {
                theme::hint(ui, &format!(
                    "This device stores profiles in format {}, which this app cannot \
                     decode. Switching between them still works; their contents are \
                     not shown, and nothing is written.",
                    self.snap.profile_format
                ));
            });
        }
        theme::section(ui, "Active profile", |ui| {
            ui.horizontal_wrapped(|ui| {
                for i in 0..self.snap.profile_count {
                    let on = self.snap.active_profile == i;
                    if ui.selectable_label(on, format!("{i}")).clicked() {
                        self.dev.send(Cmd::SetProfile(i));
                    }
                }
            });
            ui.add_space(8.0);
            theme::hint(
                ui,
                &format!(
                    "{} profiles live on the mouse. The active one applies with no \
                     software running.",
                    self.snap.profile_count
                ),
            );
        });

        if !self.snap.profiles.is_empty() {
            ui.add_space(14.0);
            self.profile_sel = self.profile_sel.min(self.snap.profiles.len() - 1);
            theme::section(ui, "Contents", |ui| {
                ui.horizontal_wrapped(|ui| {
                    for i in 0..self.snap.profiles.len() {
                        if ui
                            .selectable_label(self.profile_sel == i, format!("{i}"))
                            .clicked()
                        {
                            self.profile_sel = i;
                        }
                    }
                });
                ui.add_space(10.0);
                let p = &self.snap.profiles[self.profile_sel];
                theme::row(ui, "report rate", &format!("{} Hz", p.report_rate_hz()));
                let stages: Vec<String> =
                    p.dpi.iter().filter(|d| **d != 0).map(|d| d.to_string()).collect();
                theme::row(ui, "dpi stages", &stages.join(", "));
                theme::row(ui, "logo", &p.leds[0].describe());
                theme::row(ui, "dpi light", &p.leds[1].describe());
                theme::row(
                    ui,
                    "name",
                    if p.name.is_empty() { "(unset)" } else { &p.name },
                );
            });
        }

        if self.snap.has_profiles && self.snap.profile_format == 2 {
            ui.add_space(14.0);
            let snap = self.snap.clone();
            let sel = self.profile_sel as u8;
            for cmd in self.profile_editor.ui(ui, &snap, sel) {
                self.dev.send(cmd);
            }
        }

        ui.add_space(14.0);
        theme::section(ui, "Backup", |ui| {
            ui.horizontal(|ui| {
                let w = (ui.available_width() - 90.0).max(160.0);
                ui.add(egui::TextEdit::singleline(&mut self.backup_path).desired_width(w));
                if ui.button("Save").clicked() {
                    self.dev.send(Cmd::Backup(self.backup_path.clone()));
                }
            });
            ui.add_space(8.0);
            theme::hint(
                ui,
                "Dumps all 16 sectors including the profile directory. Take one before \
                 changing buttons or saving lighting.",
            );
        });
    }

    fn draw_buttons(&mut self, ui: &mut egui::Ui) {
        // Host bindings first: they need no flash write and are what most
        // people are looking for.
        let snap = self.snap.clone();
        for cmd in self.bindings.ui(ui, &snap) {
            self.dev.send(cmd);
        }
        // The editor also queues things Cmd could not express when it was
        // written. Draining this is what makes Save actually reach the device.
        for req in self.bindings.take_requests() {
            self.dev.send(match req {
                bindings_editor::Request::ApplyBindings { silence } => {
                    Cmd::ApplyBindings { silence }
                }
                bindings_editor::Request::RestoreIdentity => Cmd::RestoreIdentity,
            });
        }
        ui.add_space(18.0);
        if !self.snap.has_profiles {
            theme::unsupported(ui, "onboard profiles, so bindings cannot be read");
            return;
        }
        if self.snap.profiles.is_empty() {
            theme::section(ui, "Buttons", |ui| {
                theme::hint(ui, "No profile read yet. Press Refresh.");
            });
            return;
        }
        let idx = self.snap.active_profile;
        let Some(p) = self.snap.profiles.get(idx as usize).cloned() else {
            theme::section(ui, "Buttons", |ui| {
                theme::hint(ui, "Active profile not readable.");
            });
            return;
        };

        let mut choices: Vec<String> = SPECIALS.iter().map(|(_, n)| n.to_string()).collect();
        for n in 1..=5u8 {
            choices.push(format!("button {n}"));
        }
        choices.push("disabled".to_string());

        let n_buttons = self.snap.button_count;
        let mut pending: Option<(u8, Button)> = None;

        theme::section(ui, &format!("Buttons, profile {idx}"), |ui| {
            egui::Grid::new("btns")
                .num_columns(4)
                .spacing(egui::vec2(14.0, 8.0))
                .show(ui, |ui| {
                    for i in 0..n_buttons {
                        let cur = p.buttons.get(i as usize).copied().unwrap_or(Button::disabled());
                        let cur_desc = cur.describe();
                        ui.label(egui::RichText::new(format!("{i}")).color(theme::DIM).size(12.5));
                        ui.label(egui::RichText::new(&cur_desc).size(13.0));

                        let sel = self
                            .btn_action
                            .entry(i)
                            .or_insert_with(|| cur_desc.clone())
                            .clone();
                        egui::ComboBox::from_id_salt(("btn", i))
                            .selected_text(&sel)
                            .show_ui(ui, |ui| {
                                for c in &choices {
                                    let mut s = sel.clone();
                                    if ui.selectable_value(&mut s, c.clone(), c).clicked() {
                                        self.btn_action.insert(i, c.clone());
                                    }
                                }
                            });

                        let changed = sel != cur_desc;
                        if ui.add_enabled(changed, egui::Button::new("Apply")).clicked() {
                            if let Some(b) = action_to_button(&sel) {
                                pending = Some((i, b));
                            }
                        }
                        ui.end_row();
                    }
                });
            ui.add_space(10.0);
            theme::hint(
                ui,
                "Changing a binding writes onboard flash, so it persists with nothing \
                 running. Take a backup from the Profiles tab first.",
            );
        });

        if let Some((button, binding)) = pending {
            self.dev.send(Cmd::SetButton { index: idx, button, binding });
            self.btn_action.remove(&button);
        }
    }

    fn draw_service(&mut self, ui: &mut egui::Ui) {
        use crate::service::{self as svc, Action, State};
        let st = self.service.state();
        let colour = match st {
            State::Active => theme::GOOD,
            State::Failed => theme::BAD,
            State::Inactive => theme::WARN,
            _ => theme::FAINT,
        };

        theme::section(ui, "Background service", |ui| {
            ui.horizontal(|ui| {
                theme::pill(ui, svc::label(st), colour);
                if self.service.is_enabled() {
                    theme::pill(ui, "starts at login", theme::DIM);
                }
                if let Some(a) = self.service.busy() {
                    theme::pill(ui, a.describe(), theme::ACCENT);
                }
            });
            ui.add_space(8.0);
            theme::hint(ui, svc::describe(st));
            ui.add_space(12.0);

            let can = svc::can_control(st) && self.service.busy().is_none();
            ui.horizontal(|ui| {
                if ui.add_enabled(can && st != State::Active, egui::Button::new("Start")).clicked() {
                    self.service.request(Action::Start);
                }
                if ui.add_enabled(can && st == State::Active, egui::Button::new("Stop")).clicked() {
                    self.service.request(Action::Stop);
                }
                if ui.add_enabled(can, egui::Button::new("Restart")).clicked() {
                    self.service.request(Action::Restart);
                }
                let enabled = self.service.is_enabled();
                let label = if enabled { "Don't start at login" } else { "Start at login" };
                if ui.add_enabled(can, egui::Button::new(label)).clicked() {
                    self.service.request(if enabled { Action::Disable } else { Action::Enable });
                }
            });

            if st == State::NotInstalled {
                ui.add_space(10.0);
                theme::hint(
                    ui,
                    "The unit file is not installed. Run packaging/install.sh once; it \
                     needs sudo for the udev rules, which is why the app cannot do it.",
                );
            }
            if let Some(e) = self.service.last_error() {
                ui.add_space(8.0);
                ui.label(egui::RichText::new(e).color(theme::BAD).size(12.5));
            }
            if let Some(n) = self.service.last_notice() {
                ui.add_space(8.0);
                ui.label(egui::RichText::new(n).color(theme::GOOD).size(12.5));
            }
        });

        theme::section(ui, "Recent log", |ui| {
            let log = self.service.log();
            if log.is_empty() {
                theme::hint(ui, "Nothing logged yet.");
            } else {
                egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                    for line in log {
                        ui.label(egui::RichText::new(line).monospace().size(11.5).color(theme::DIM));
                    }
                });
            }
            ui.add_space(8.0);
            if ui.button("Refresh").clicked() {
                self.service.refresh();
            }
        });
    }

    fn draw_about(&mut self, ui: &mut egui::Ui) {
        theme::section(ui, "Device", |ui| {
            theme::row(ui, "model", &self.snap.model);
            theme::row(ui, "node", &self.snap.path);
            theme::row(ui, "HID++", &self.snap.protocol);
            theme::row(ui, "mode", crate::profiles::mode_name(self.snap.mode));
        });
        ui.add_space(14.0);
        theme::section(ui, "What this hardware can and cannot do", |ui| {
            for line in [
                "The firmware implements four lighting modes: off, fixed, breathing, cycle.",
                "Wave, starlight, ripple and frame streaming are in HID++ but absent here.",
                "Only fixed and breathing can be set live; cycle has to be stored or streamed.",
                "Animated effects are streamed from this app and stop when it stops.",
                "DPI is continuous, 100-25600 in steps of 50.",
                "Report rate cannot be set live in onboard mode, so it is written to flash.",
                "Surface tuning and lift-off distance are out of scope: they sit behind",
                "engineering-flagged features that can brick the mouse.",
            ] {
                theme::hint(ui, line);
            }
            ui.add_space(8.0);
            theme::hint(ui, "Protocol detail is in PROTOCOL.md.");
        });
    }
}

pub fn run() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([940.0, 700.0])
            .with_min_inner_size([720.0, 480.0])
            .with_title("Logi"),
        ..Default::default()
    };
    eframe::run_native("logi", options, Box::new(|_cc| Ok(Box::new(App::new()))))
}
