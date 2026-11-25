//! The "MIDI output" area of the Settings tab: the port the rehearsal
//! player sends the MIDI file of a song to, an optional second port that
//! gets exactly the same messages (e.g. the LED show simulator), and a test
//! button (it sends to both). The choice
//! goes into `Config` (saved by the caller). The logic (ports, sending, the
//! messages of the test) is in `lp16_tool::midi_out` and
//! `lp16_tool::rehearsal_midi`; this only draws and keeps the list of ports.

use crate::gui_app::{panel_body, panel_tab, small_btn, DIM, ORANGE, RED};
use eframe::egui::{self, RichText, Ui};
use lp16_tool::tr;
use lp16_tool::midi_out::{list_midi_outputs, PortStatus};
use lp16_tool::Config;
use std::time::Duration;

/// What the user did in the area during this frame.
#[derive(Default)]
pub struct MidiOutcome {
    /// the port was changed: save the config and tell the player
    pub changed: bool,
    /// "Send test" was pressed
    pub test: bool,
}

#[derive(Default)]
pub struct MidiPanel {
    ports: Vec<String>,
    /// the list was read at least once (when the area is shown first)
    listed: bool,
    list_error: Option<String>,
    /// what happened to the last test
    test_note: Option<String>,
}

impl MidiPanel {
    /// Read the ports of the system again.
    pub fn refresh(&mut self) {
        self.listed = true;
        match list_midi_outputs() {
            Ok(p) => {
                self.ports = p;
                self.list_error = None;
            }
            Err(e) => {
                self.ports.clear();
                self.list_error = Some(e);
            }
        }
    }

    /// Tell the user how the last test went.
    pub fn set_test_note(&mut self, note: Option<String>) {
        self.test_note = note;
    }

    /// `can_test`: a song with a MIDI file is loaded; `status` / `status2`
    /// the state of the first / second port (an error is shown here as well
    /// as in the Rehearsal tab).
    pub fn ui(
        &mut self,
        ui: &mut Ui,
        config: &mut Config,
        can_test: bool,
        status: &PortStatus,
        status2: &PortStatus,
    ) -> MidiOutcome {
        let mut out = MidiOutcome::default();
        if !self.listed {
            self.refresh();
        }
        let mut refresh = false;
        panel_tab(ui, tr!("midi.title"), false);
        panel_body(ui, false, |ui| {
            let chosen = config.midi_port.clone();
            let shown = if chosen.is_empty() {
                tr!("common.none").to_string()
            } else if self.ports.contains(&chosen) {
                chosen.clone()
            } else {
                tr!("common.not_found", name = chosen)
            };
            ui.horizontal(|ui| {
                ui.label(tr!("midi.port"));
                egui::ComboBox::from_id_source("midi_port").selected_text(shown).width(300.0).show_ui(ui, |ui| {
                    if ui.selectable_label(chosen.is_empty(), tr!("common.none")).clicked() && !chosen.is_empty() {
                        config.midi_port.clear();
                        out.changed = true;
                    }
                    for p in &self.ports {
                        if ui.selectable_label(chosen == *p, p).clicked() && chosen != *p {
                            config.midi_port = p.clone();
                            out.changed = true;
                        }
                    }
                });
                if small_btn(ui, tr!("common.refresh"), true) {
                    refresh = true;
                }
            });
            // the optional second output: same list, same refresh
            let chosen2 = config.midi_port2.clone();
            let shown2 = if chosen2.is_empty() {
                tr!("common.none").to_string()
            } else if self.ports.contains(&chosen2) {
                chosen2.clone()
            } else {
                tr!("common.not_found", name = chosen2)
            };
            ui.horizontal(|ui| {
                ui.label(tr!("midi.port2"));
                egui::ComboBox::from_id_source("midi_port2").selected_text(shown2).width(300.0).show_ui(ui, |ui| {
                    if ui.selectable_label(chosen2.is_empty(), tr!("common.none")).clicked() && !chosen2.is_empty() {
                        config.midi_port2.clear();
                        out.changed = true;
                    }
                    for p in &self.ports {
                        if ui.selectable_label(chosen2 == *p, p).clicked() && chosen2 != *p {
                            config.midi_port2 = p.clone();
                            out.changed = true;
                        }
                    }
                });
            });
            ui.label(RichText::new(tr!("midi.port2_tip")).size(11.0).color(DIM));
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if small_btn(ui, tr!("midi.send_test"), can_test && !config.midi_port.is_empty()) {
                    out.test = true;
                }
                let note = if config.midi_port.is_empty() {
                    tr!("midi.choose_port")
                } else if !can_test {
                    tr!("midi.load_song")
                } else {
                    tr!("midi.test_tip")
                };
                ui.label(RichText::new(note).size(11.0).color(DIM));
            });
            if let Some(n) = &self.test_note {
                ui.add_space(4.0);
                ui.label(RichText::new(n).size(12.0).color(ORANGE));
            }
            for e in [
                self.list_error.as_deref(),
                status.error.as_deref().filter(|_| !config.midi_port.is_empty()),
                status2.error.as_deref().filter(|_| !config.midi_port2.is_empty()),
            ]
                .into_iter()
                .flatten()
            {
                ui.add_space(4.0);
                ui.add(egui::Label::new(RichText::new(e).size(12.5).color(RED)).wrap(true));
            }
            ui.label(
                RichText::new(
                    "The MIDI file of a song is sent in time with the audio. The port is opened at the next Play \
                     (or by the test). XR18: MIDI over USB to the XR18, from its MIDI out to the modeler.",
                )
                .size(11.0)
                .color(DIM),
            );
        });
        if refresh {
            self.refresh();
        }
        if out.changed {
            self.test_note = None;
        }
        if out.test {
            // the port opens on the MIDI thread: look again shortly
            ui.ctx().request_repaint_after(Duration::from_millis(300));
        }
        out
    }
}
