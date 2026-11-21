//! Follow a log file as it grows (`tail -F`), without opening it in the
//! editor: filter, pause, and keep the newest lines.

use std::collections::VecDeque;

use egui::RichText;
use tokio::sync::oneshot;

/// Lines kept; older ones are dropped.
const KEEP: usize = 20_000;

pub struct LogView {
    pub id: u64,
    pub path: String,
    pub sudo: bool,
    lines: VecDeque<String>,
    partial: String,
    filter: String,
    /// While paused, new lines are still collected but the view stays put.
    paused: bool,
    wrap: bool,
    pub ended: Option<String>,
    /// Dropping this stops `tail` on the server.
    pub stop: Option<oneshot::Sender<()>>,
    dropped: usize,
}

pub enum LogAction {
    /// Start again (after it ended, or with sudo).
    Restart,
}

impl LogView {
    pub fn new(id: u64, path: String, sudo: bool, stop: oneshot::Sender<()>) -> Self {
        Self {
            id,
            path,
            sudo,
            lines: VecDeque::new(),
            partial: String::new(),
            filter: String::new(),
            paused: false,
            wrap: false,
            ended: None,
            stop: Some(stop),
            dropped: 0,
        }
    }

    pub fn title(&self) -> String {
        format!("📄 {}", malasdoh_core::sftp::file_name(&self.path))
    }

    pub fn push(&mut self, data: &[u8]) {
        self.partial.push_str(&String::from_utf8_lossy(data));
        while let Some(i) = self.partial.find('\n') {
            let mut line: String = self.partial.drain(..=i).collect();
            line.pop();
            if line.ends_with('\r') {
                line.pop();
            }
            self.lines.push_back(line);
        }
        while self.lines.len() > KEEP {
            self.lines.pop_front();
            self.dropped += 1;
        }
    }

    pub fn end(&mut self, why: String) {
        if !self.partial.is_empty() {
            let rest = std::mem::take(&mut self.partial);
            self.lines.push_back(rest);
        }
        self.stop = None;
        self.ended = Some(why);
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) -> Option<LogAction> {
        let mut action = None;
        ui.horizontal(|ui| {
            ui.monospace(&self.path);
            if self.sudo {
                ui.label(RichText::new("via sudo").weak());
            }
            match &self.ended {
                None => {
                    ui.spinner().on_hover_text("Following; new lines appear as they are written");
                }
                Some(why) => {
                    ui.colored_label(ui.visuals().warn_fg_color, why);
                    if ui.button("⟳ Follow again").clicked() {
                        action = Some(LogAction::Restart);
                    }
                }
            }
        });
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text("Show only lines containing…")
                    .desired_width(crate::dlg::field_width(ui, 260.0)),
            );
            ui.toggle_value(&mut self.paused, "Pause").on_hover_text("Stop scrolling; lines are still collected");
            ui.checkbox(&mut self.wrap, "Wrap");
            if ui.button("Clear").clicked() {
                self.lines.clear();
                self.dropped = 0;
            }
            if ui.button("Copy shown").clicked() {
                let text: Vec<&str> = self.shown().collect();
                ui.ctx().copy_text(text.join("\n"));
            }
            let n = self.lines.len();
            let mut info = format!("{n} lines");
            if self.dropped > 0 {
                info.push_str(&format!(" (oldest {} dropped)", self.dropped));
            }
            ui.label(RichText::new(info).weak());
        });
        ui.separator();

        let shown: Vec<&str> = self.shown().collect();
        let warn = ui.visuals().warn_fg_color;
        let err = ui.visuals().error_fg_color;
        let row_h = ui.text_style_height(&egui::TextStyle::Monospace) + 2.0;
        let mut area = if self.wrap { egui::ScrollArea::vertical() } else { egui::ScrollArea::both() };
        area = area.id_salt(("log", self.id)).auto_shrink([false, false]).stick_to_bottom(!self.paused);
        let paint = |ui: &mut egui::Ui, l: &str| {
            let lower = l.to_ascii_lowercase();
            let mut t = RichText::new(l).monospace();
            if lower.contains("error") || lower.contains("fatal") || lower.contains("crit") {
                t = t.color(err);
            } else if lower.contains("warn") {
                t = t.color(warn);
            }
            ui.label(t);
        };
        if self.wrap {
            // Wrapped lines have different heights, so no row virtualisation.
            area.show(ui, |ui| {
                for l in shown.iter().rev().take(5000).rev() {
                    paint(ui, l);
                }
            });
        } else {
            area.show_rows(ui, row_h, shown.len(), |ui, range| {
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                ui.spacing_mut().item_spacing.y = 2.0;
                for l in &shown[range] {
                    paint(ui, l);
                }
            });
        }
        action
    }

    fn shown(&self) -> impl Iterator<Item = &str> {
        let f = self.filter.to_lowercase();
        self.lines.iter().map(String::as_str).filter(move |l| f.is_empty() || l.to_lowercase().contains(&f))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_across_chunks_and_limit() {
        let (tx, _rx) = oneshot::channel();
        let mut v = LogView::new(1, "/var/log/x".into(), false, tx);
        v.push(b"one\ntw");
        v.push(b"o\r\nthree");
        assert_eq!(v.lines, ["one", "two"]);
        v.end("ended".into());
        assert_eq!(v.lines, ["one", "two", "three"]);
        for i in 0..KEEP + 5 {
            v.push(format!("{i}\n").as_bytes());
        }
        assert_eq!((v.lines.len(), v.dropped), (KEEP, 8));
        v.filter = "1999".into();
        assert!(v.shown().all(|l| l.contains("1999")));
    }
}
