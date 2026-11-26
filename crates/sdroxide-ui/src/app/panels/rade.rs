//! The FreeDV / RADE digital-voice panel.
//!
//! There is no text to show and no decode list to draw, so the panel is a
//! status view: sync state, SNR, and the reporting the mode does to FreeDV
//! Reporter on the operator's behalf.

use crate::i18n::tr;
use eframe::egui::{self, RichText};
use sdroxide_types::{Command, RadeVersion};

use crate::app::{SdroxideApp, tx_gated};

/// How far from our own frequency a station may be and still be listed.
const NEARBY_HZ: u64 = 1_000;
/// Column positions of the station list, from its left edge. The first five
/// are narrow enough for a phone.
const SB_X: f32 = 100.0;
const STATUS_X: f32 = 140.0;
const SNR_RIGHT_X: f32 = 230.0;
const KM_RIGHT_X: f32 = 315.0;
/// Where software and message go: on the same line from [`ONE_LINE_W`] on,
/// otherwise on a second one.
const VERSION_X: f32 = 335.0;
const VERSION_W: f32 = 245.0;
const ONE_LINE_W: f32 = 800.0;
const ROW_H: f32 = 18.0;

/// One line of the station list: the text of each column.
#[derive(Default)]
struct StationCells<'a> {
    call: &'a str,
    sideband: &'a str,
    /// The station says it is on the other sideband than ours: marked, since
    /// we have to switch to hear it.
    other_sideband: bool,
    status: &'a str,
    snr: &'a str,
    km: &'a str,
    version: &'a str,
    message: &'a str,
}

fn station_row(
    ui: &mut egui::Ui,
    tint: Option<egui::Color32>,
    c: &StationCells,
    color: egui::Color32,
) -> egui::Response {
    let width = ui.available_width();
    let one_line = width >= ONE_LINE_W;
    let two_lines = !one_line && !(c.version.is_empty() && c.message.is_empty());
    let height = if two_lines { 2.0 * ROW_H } else { ROW_H };
    // No gap between rows: the tint is what separates them.
    let (r, resp) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    let p = ui.painter_at(r);
    if let Some(tint) = tint {
        p.rect_filled(r, 0.0, tint);
    }
    let font = egui::FontId::monospace(12.0);
    let first = r.top() + ROW_H / 2.0;
    let left = egui::Align2::LEFT_CENTER;
    let right = egui::Align2::RIGHT_CENTER;
    for (text, x, align) in [
        (c.call, 4.0, left),
        (c.status, STATUS_X, left),
        (c.snr, SNR_RIGHT_X, right),
        (c.km, KM_RIGHT_X, right),
    ] {
        p.text(egui::pos2(r.left() + x, first), align, text, font.clone(), color);
    }
    // Filled where marked: a text colour alone would be lost on the row tints.
    let ink = if c.other_sideband { crate::theme::INK_ON_CYAN() } else { color };
    let sideband = p.layout_no_wrap(c.sideband.to_string(), font.clone(), ink);
    let at = egui::pos2(r.left() + SB_X, first - sideband.size().y / 2.0);
    if c.other_sideband {
        let chip = egui::Rect::from_min_size(at, sideband.size()).expand2(egui::vec2(2.0, 0.0));
        p.rect_filled(chip, 2.0, crate::theme::CYAN());
    }
    p.galley(at, sideband, ink);
    if one_line {
        // Clipped to its column, so a long one cannot run into the message.
        let column = egui::Rect::from_min_size(
            egui::pos2(r.left() + VERSION_X, r.top()),
            egui::vec2(VERSION_W, ROW_H),
        );
        ui.painter_at(column).text(column.left_center(), left, c.version, font.clone(), color);
        let x = r.left() + VERSION_X + VERSION_W + 12.0;
        p.text(egui::pos2(x, first), left, c.message, font, color);
    } else if two_lines {
        let text = match (c.version.is_empty(), c.message.is_empty()) {
            (false, false) => format!("{} · {}", c.message, c.version),
            (false, true) => c.version.to_string(),
            _ => c.message.to_string(),
        };
        let at = egui::pos2(r.left() + 16.0, first + ROW_H);
        p.text(at, left, text, egui::FontId::monospace(11.0), color);
    }
    resp
}

impl SdroxideApp {
    pub(in crate::app) fn rade_panel(
        &mut self,
        ui: &mut egui::Ui,
        cmds: &mut Vec<Command>,
        _panel_h: f32,
    ) {
        let status = self.digi_status.clone();
        let rade = status.as_ref().and_then(|s| s.rade).unwrap_or_default();
        let transmitting = status.as_ref().map(|s| s.transmitting).unwrap_or(false);
        // The last callsign decoded from a remote End-of-Over frame. The
        // controller has always carried it in `dx_call` (and reported it to
        // FreeDV Reporter from the same place) — this is where it reaches the
        // operator. It is `None` while transmitting: our own over is not a
        // station being heard.
        let dx_call = status.as_ref().and_then(|s| s.dx_call.clone());
        let version = self.digi_cfg_edit.rade_version;
        let v2 = version == RadeVersion::V2;

        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new(tr!("rade-rade")).size(11.0).strong().color(crate::theme::CYAN()),
            );
            ui.label(
                RichText::new(if v2 {
                    tr!("rade-freedv-v2-digital-voice")
                } else {
                    tr!("rade-freedv-v1-digital-voice")
                })
                .size(10.5)
                .color(crate::theme::CYAN_DIM()),
            );
            ui.add_space(8.0);
            // Which waveform. Held while transmitting: the modem only changes
            // between overs. "V1" and "V2" are names, so not translated.
            ui.add_enabled_ui(!transmitting && self.digi_cfg_seeded, |ui| {
                ui.horizontal(|ui| {
                    for (choice, name) in [(RadeVersion::V1, "V1"), (RadeVersion::V2, "V2")] {
                        let resp = crate::chrome::chip(
                            ui,
                            version == choice,
                            RichText::new(name).size(10.5),
                        );
                        if resp.clicked() && version != choice {
                            self.digi_cfg_edit.rade_version = choice;
                            cmds.push(Command::SetDigiConfig(self.digi_cfg_edit.clone()));
                        }
                        resp.on_hover_text(tr!("rade-version-tip"));
                    }
                    ui.add_space(8.0);
                    // Which sideband, shown as it is in effect: the band's
                    // usual one unless chosen otherwise here.
                    let lower = self
                        .state
                        .is_lower_sideband(sdroxide_types::Mode::Rade, self.state.rx_freq_hz());
                    for (choice, name) in [(true, "LSB"), (false, "USB")] {
                        let resp = crate::chrome::chip(
                            ui,
                            lower == choice,
                            RichText::new(name).size(10.5),
                        );
                        if resp.clicked() && lower != choice {
                            cmds.push(Command::SetRadeSideband(choice));
                        }
                        resp.on_hover_text(tr!("rade-sideband-tip"));
                    }
                    ui.add_space(8.0);
                    // The narrow filter on the transmit audio, for either
                    // version.
                    let filtered = self.digi_cfg_edit.rade_tx_filter;
                    let resp = crate::chrome::chip(
                        ui,
                        filtered,
                        RichText::new(tr!("rade-tx-filter")).size(10.5),
                    );
                    if resp.clicked() {
                        self.digi_cfg_edit.rade_tx_filter = !filtered;
                        cmds.push(Command::SetDigiConfig(self.digi_cfg_edit.clone()));
                    }
                    resp.on_hover_text(tr!("rade-tx-filter-tip"));
                });
            });
            crate::chrome::row_tail(ui, |ui| {
                if transmitting {
                    ui.label(
                        RichText::new(tr!("rade-tx"))
                            .size(11.0)
                            .strong()
                            .color(crate::theme::ALERT()),
                    );
                    ui.add_space(8.0);
                }
                // Silence the raw signal, leaving only decoded speech audible.
                let muted = self.digi_cfg_edit.rade_mute_analog;
                let resp = crate::chrome::chip(
                    ui,
                    muted,
                    RichText::new(tr!("rade-mute-analog")).size(10.5),
                );
                if resp.clicked() && self.digi_cfg_seeded {
                    self.digi_cfg_edit.rade_mute_analog = !muted;
                    cmds.push(Command::SetDigiConfig(self.digi_cfg_edit.clone()));
                }
                resp.on_hover_text(tr!("rade-mute-demodulated-audio-so-only"));
            });
        });
        ui.add_space(8.0);

        // Sync lamp + link readouts.
        ui.horizontal(|ui| {
            let (lamp, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
            let lit = rade.sync && !transmitting;
            ui.painter_at(lamp).circle_filled(
                lamp.center(),
                5.5,
                if lit { crate::theme::GREEN() } else { crate::theme::gray(48) },
            );
            ui.label(
                RichText::new(if transmitting {
                    tr!("rade-transmitting")
                } else if rade.sync {
                    tr!("rade-sync")
                } else {
                    String::from("searching")
                })
                .size(12.0)
                .strong()
                .color(if lit {
                    crate::theme::GREEN()
                } else {
                    crate::theme::gray(130)
                }),
            );
            ui.add_space(16.0);
            let dim = crate::theme::gray(150);
            if rade.sync && !transmitting {
                ui.label(
                    RichText::new(tr!("rade-snr-db", snr_db = format!("{:.0}", rade.snr_db)))
                        .size(12.0)
                        .color(crate::theme::TEXT_STRONG()),
                );
                ui.add_space(12.0);
                ui.label(
                    RichText::new(tr!(
                        "rade-offset-hz",
                        freq_offset_hz = format!("{:+.0}", rade.freq_offset_hz)
                    ))
                    .size(11.0)
                    .color(dim),
                )
                .on_hover_text(tr!("rade-how-far-received-signal-sits"));
            } else {
                ui.label(RichText::new(tr!("rade-snr")).size(12.0).color(dim));
            }
            if rade.mistune != 0 && !rade.sync && !transmitting {
                ui.add_space(12.0);
                ui.label(
                    RichText::new(if rade.mistune > 0 {
                        tr!("rade-signal-too-high")
                    } else {
                        tr!("rade-signal-too-low")
                    })
                    .size(12.0)
                    .strong()
                    .color(crate::theme::YELLOW()),
                )
                .on_hover_text(tr!("rade-mistune-tip"));
            }
            // Who we last heard, from the End-of-Over frame that closed their
            // over — the only time a RADE station names itself. Held for a
            // minute past the signal, then dropped by the controller.
            if let Some(call) = &dx_call {
                ui.add_space(12.0);
                ui.label(
                    RichText::new(tr!("rade-heard", call = call))
                        .size(12.0)
                        .strong()
                        .color(crate::theme::GREEN()),
                )
                .on_hover_text(if v2 {
                    tr!("rade-callsign-station-sends-v2")
                } else {
                    tr!("rade-callsign-station-put-end-over")
                });
            }
        });
        ui.add_space(8.0);

        // Speech level: decoded on receive, into the vocoder on transmit.
        {
            let (bar, _) =
                ui.allocate_exact_size(egui::vec2(ui.available_width(), 6.0), egui::Sense::hover());
            let p = ui.painter_at(bar);
            p.rect_filled(bar, 0.0, crate::theme::gray(22));
            let level = rade.rx_level.clamp(0.0, 1.0);
            if level > 0.0 {
                let mut fill = bar;
                fill.set_width(bar.width() * level);
                p.rect_filled(fill, 0.0, crate::theme::CYAN());
            }
        }
        ui.add_space(12.0);

        // Transmit. `DigiTxActive` is the same command the main PTT button ends
        // up sending in this mode, so the two stay in step.
        let tx_ok = self.tx_capable();
        ui.horizontal(|ui| {
            let label = if transmitting { tr!("rade-stop-talking") } else { tr!("rade-talk-ptt") };
            let resp = tx_gated(ui, tx_ok, |ui| {
                crate::chrome::chip_accent(
                    ui,
                    transmitting,
                    RichText::new(label).size(13.0).strong(),
                    crate::theme::ALERT(),
                    crate::theme::TEXT_STRONG(),
                )
            });
            if resp.clicked() {
                cmds.push(Command::DigiTxActive(!transmitting));
            }
            resp.on_hover_text(tr!("rade-open-close-rade-over-modem"));
            ui.add_space(10.0);
        });
        ui.add_space(12.0);

        // The station list and the notes below it share what height is left.
        egui::ScrollArea::vertical().id_salt("rade-stations").auto_shrink(false).show(ui, |ui| {
            self.rade_station_list(ui);
            ui.add_space(12.0);
            ui.label(
                RichText::new(if v2 {
                    tr!("rade-v2-experimental-note")
                } else {
                    tr!("rade-rade-v1-occupies-roughly-1060")
                })
                .size(10.5)
                .color(crate::theme::gray(125)),
            );
            if rade.dropped > 0 {
                ui.add_space(6.0);
                ui.label(
                    RichText::new(tr!(
                        "rade-samples-dropped-between-receiver-decoder",
                        dropped = rade.dropped
                    ))
                    .size(10.5)
                    .color(crate::theme::YELLOW()),
                );
            }
        });
    }

    /// Who else FreeDV Reporter has on this frequency, nearest first. A row is
    /// tinted while that station transmits, or while it reports receiving
    /// something.
    fn rade_station_list(&self, ui: &mut egui::Ui) {
        let ours = self.state.tx_freq_hz().round().max(0.0) as u64;
        let my_grid = &self.digi_cfg_edit.my_grid;
        // Both sidebands are listed: many stations do not say which they are
        // on, and guessing would hide some of those worth trying.
        let our_lsb =
            self.state.is_lower_sideband(sdroxide_types::Mode::Rade, self.state.tx_freq_hz());
        let mut rows: Vec<_> = self
            .freedv_stations
            .iter()
            .filter(|s| s.freq_hz.abs_diff(ours) <= NEARBY_HZ)
            .map(|s| (sdroxide_types::grid_distance_km(my_grid, &s.grid), s))
            .collect();
        if rows.is_empty() {
            ui.label(
                RichText::new(tr!("rade-no-stations")).size(11.0).color(crate::theme::gray(125)),
            );
            return;
        }
        // Unknown distances last. The list arrives sorted by callsign, and the
        // sort is stable, so equal distances stay in that order.
        rows.sort_by(|a, b| a.0.unwrap_or(f64::MAX).total_cmp(&b.0.unwrap_or(f64::MAX)));

        ui.spacing_mut().item_spacing.y = 1.0;
        let wide = ui.available_width() >= ONE_LINE_W;
        let (call, status, snr) =
            (tr!("rade-col-call"), tr!("rade-col-status"), tr!("rade-col-snr"));
        let (version, message) = (tr!("rade-col-version"), tr!("rade-col-message"));
        let header = StationCells {
            call: &call,
            sideband: "SB",
            other_sideband: false,
            status: &status,
            snr: &snr,
            km: "km",
            // The second line of a narrow list goes without a heading.
            version: if wide { &version } else { "" },
            message: if wide { &message } else { "" },
        };
        station_row(ui, None, &header, crate::theme::gray(125));
        for (km, s) in rows {
            // Transmitting wins, as on the reporter's own page.
            let (tint, status, snr) = if s.tx {
                (Some(crate::theme::ALERT().gamma_multiply(0.35)), "TX", None)
            } else {
                // Yellow for a weak one, at or below 0 dB.
                let tint = s.rx_snr_db.map(|db| {
                    let color = if db > 0 { crate::theme::GREEN() } else { crate::theme::YELLOW() };
                    color.gamma_multiply(0.30)
                });
                (tint, "RX", s.rx_snr_db)
            };
            let snr = snr.map(|db| format!("{db} dB")).unwrap_or_default();
            let km = km.map(|km| format!("{km:.0}")).unwrap_or_default();
            // Free text from a stranger: keep it to its one line.
            let message = s.message.replace(['\r', '\n'], " ");
            let cells = StationCells {
                call: &s.call,
                // "?" where it does not say.
                sideband: match s.lsb {
                    Some(true) => "LSB",
                    Some(false) => "USB",
                    None => "?",
                },
                other_sideband: s.lsb.is_some_and(|lower| lower != our_lsb),
                status,
                snr: &snr,
                km: &km,
                version: &s.version,
                message: &message,
            };
            let resp = station_row(ui, tint, &cells, crate::theme::TEXT_STRONG());
            // Whatever the width cut off.
            if !message.is_empty() || !s.version.is_empty() {
                resp.on_hover_text(format!("{message}\n{}", s.version).trim().to_string());
            }
        }
    }
}
