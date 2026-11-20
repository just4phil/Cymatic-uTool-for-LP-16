//! The "Audio output" area of the Settings tab: host, device, the two
//! output channels of the stereo mix, buffer size and a test tone. The
//! choices go into `Config` (saved by the caller). The logic (hosts,
//! devices, opening, channel layout, tone) is in `lp16_tool::audio_out`;
//! this only draws and keeps the lists of what is available.

use crate::gui_app::{panel_body, panel_tab, small_btn, DIM, ORANGE, RED, TEXT};
use eframe::egui::{self, RichText, Ui};
use lp16_tool::audio_out::{
    default_host_name, list_devices, list_hosts, start_test_tone, AudioSettings, DeviceInfo, StreamInfo, ToneStream,
};
use lp16_tool::{Config, AUDIO_BUFFERS};
use std::time::Duration;

/// What the user did in the area during this frame.
#[derive(Default)]
pub struct AudioOutcome {
    /// a setting was changed: save the config
    pub changed: bool,
    /// "Test tone" was pressed: call `AudioPanel::start_tone` (after making
    /// sure that the player does not hold the device)
    pub tone: bool,
}

#[derive(Default)]
pub struct AudioPanel {
    /// names of the audio hosts that are compiled in
    hosts: Vec<String>,
    /// output devices of the selected host
    devices: Vec<DeviceInfo>,
    /// the lists were read at least once (they are read when the area is
    /// shown first, never at program start)
    listed: bool,
    list_error: Option<String>,
    tone: Option<ToneStream>,
    /// the last error of the test tone
    tone_error: Option<String>,
}

impl AudioPanel {
    /// Read hosts and devices again.
    pub fn refresh(&mut self, config: &Config) {
        self.hosts = list_hosts();
        self.listed = true;
        match list_devices(&config.audio_host) {
            Ok(d) => {
                self.devices = d;
                self.list_error = None;
            }
            Err(e) => {
                self.devices.clear();
                self.list_error = Some(e);
            }
        }
    }

    /// Play the test tone on the output of `config`.
    pub fn start_tone(&mut self, config: &Config) {
        self.tone = None;
        match start_test_tone(&AudioSettings::from(config)) {
            Ok(t) => {
                self.tone = Some(t);
                self.tone_error = None;
            }
            Err(e) => self.tone_error = Some(e),
        }
    }

    /// Channels of the selected device (the default device if none is
    /// chosen); at least the two of a stereo pair and the values in use.
    fn channel_count(&self, config: &Config) -> u16 {
        let device = if config.audio_device.is_empty() {
            self.devices.iter().find(|d| d.is_default)
        } else {
            self.devices.iter().find(|d| d.name == config.audio_device)
        };
        match device {
            Some(d) => d.max_channels.max(2),
            None => 2.max(config.audio_out_left).max(config.audio_out_right),
        }
    }

    /// `running` is the stream of the player, if one is open; `playing`
    /// whether a song plays (no test tone then); `player_error` the last
    /// error of the player, shown here as well as in the Rehearsal tab.
    pub fn ui(
        &mut self,
        ui: &mut Ui,
        config: &mut Config,
        running: Option<&StreamInfo>,
        playing: bool,
        player_error: Option<&str>,
    ) -> AudioOutcome {
        let mut out = AudioOutcome::default();
        if !self.listed {
            self.refresh(config);
        }
        if self.tone.as_ref().map(|t| t.is_done()).unwrap_or(false) {
            self.tone = None;
        }
        let mut refresh = false;
        panel_tab(ui, "Audio output", false);
        panel_body(ui, false, |ui| {
            let host = if config.audio_host.trim().is_empty() { default_host_name() } else { config.audio_host.clone() };
            let channels = self.channel_count(config);
            egui::Grid::new("audio_output").num_columns(3).spacing([8.0, 6.0]).show(ui, |ui| {
                ui.label("Host:");
                egui::ComboBox::from_id_source("audio_host").selected_text(host.clone()).width(260.0).show_ui(ui, |ui| {
                    let mut names = self.hosts.clone();
                    if !names.contains(&host) {
                        names.push(host.clone()); // a saved host that this build does not have
                    }
                    for name in names {
                        if ui.selectable_label(name == host, &name).clicked() && name != host {
                            config.audio_host = name;
                            config.audio_device.clear(); // the device belongs to the host
                            out.changed = true;
                            refresh = true;
                        }
                    }
                });
                ui.label("");
                ui.end_row();

                ui.label("Device:");
                let shown = if config.audio_device.is_empty() {
                    match self.devices.iter().find(|d| d.is_default) {
                        Some(d) => format!("Default device ({})", d.name),
                        None => "Default device".to_string(),
                    }
                } else if self.devices.iter().any(|d| d.name == config.audio_device) {
                    config.audio_device.clone()
                } else {
                    format!("{} (not found)", config.audio_device)
                };
                egui::ComboBox::from_id_source("audio_device").selected_text(shown).width(260.0).show_ui(ui, |ui| {
                    if ui.selectable_label(config.audio_device.is_empty(), "Default device").clicked()
                        && !config.audio_device.is_empty()
                    {
                        config.audio_device.clear();
                        out.changed = true;
                    }
                    for d in &self.devices {
                        let label = format!("{} ({} ch)", d.name, d.max_channels);
                        if ui.selectable_label(config.audio_device == d.name, label).clicked() && config.audio_device != d.name {
                            config.audio_device = d.name.clone();
                            // fewer channels than the pair needs: back to 1 and 2
                            if d.max_channels < config.audio_out_left.max(config.audio_out_right) {
                                config.audio_out_left = 1;
                                config.audio_out_right = 2.min(d.max_channels.max(1));
                            }
                            out.changed = true;
                        }
                    }
                });
                if small_btn(ui, "Refresh", true) {
                    refresh = true;
                }
                ui.end_row();

                for (label, id) in [("Left:", "audio_left"), ("Right:", "audio_right")] {
                    ui.label(label);
                    let value = if id == "audio_left" { &mut config.audio_out_left } else { &mut config.audio_out_right };
                    egui::ComboBox::from_id_source(id).selected_text(format!("Channel {}", value)).width(120.0).show_ui(ui, |ui| {
                        for c in 1..=channels.max(*value) {
                            if ui.selectable_label(*value == c, format!("Channel {}", c)).clicked() && *value != c {
                                *value = c;
                                out.changed = true;
                            }
                        }
                    });
                    ui.label(
                        RichText::new(if id == "audio_left" {
                            "output channel of the left side of the stereo mix"
                        } else {
                            "output channel of the right side"
                        })
                        .size(11.0)
                        .color(DIM),
                    );
                    ui.end_row();
                }

                ui.label("Buffer:");
                let buffer_text = |b: u32| if b == 0 { "Driver default".to_string() } else { format!("{} frames", b) };
                egui::ComboBox::from_id_source("audio_buffer")
                    .selected_text(buffer_text(config.audio_buffer))
                    .width(120.0)
                    .show_ui(ui, |ui| {
                        for b in AUDIO_BUFFERS {
                            if ui.selectable_label(config.audio_buffer == b, buffer_text(b)).clicked() && config.audio_buffer != b {
                                config.audio_buffer = b;
                                out.changed = true;
                            }
                        }
                    });
                ui.label(RichText::new("smaller = less delay, larger = fewer dropouts").size(11.0).color(DIM));
                ui.end_row();
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let can_tone = !playing && self.tone.is_none();
                if small_btn(ui, "Test tone", can_tone) {
                    out.tone = true;
                }
                let note = if self.tone.is_some() {
                    "1 kHz, 1 s on the left channel, then 1 s on the right channel."
                } else if playing {
                    "Not while a song is playing."
                } else {
                    "Plays 1 kHz at -20 dBFS, left then right, on the output chosen above."
                };
                ui.label(RichText::new(note).size(11.0).color(DIM));
            });
            if let Some(info) = running {
                ui.add_space(4.0);
                ui.label(RichText::new(format!("Running: {} ({})", info.describe(), info.device)).size(12.0).color(TEXT));
                if let Some(note) = &info.note {
                    ui.label(RichText::new(note).size(12.0).color(ORANGE));
                }
            }
            if let Some(t) = &self.tone {
                if let Some(note) = &t.info().note {
                    ui.label(RichText::new(note).size(12.0).color(ORANGE));
                }
            }
            for e in [self.list_error.as_deref(), self.tone_error.as_deref(), player_error].into_iter().flatten() {
                ui.add_space(4.0);
                ui.add(egui::Label::new(RichText::new(e).size(12.5).color(RED)).wrap(true));
            }
            ui.label(
                RichText::new(
                    "Saved in lp16-tool.toml. A running stream is opened again with the new settings at the next Play.",
                )
                .size(11.0)
                .color(DIM),
            );
        });
        if refresh {
            self.refresh(config);
        }
        if self.tone.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
        out
    }
}
