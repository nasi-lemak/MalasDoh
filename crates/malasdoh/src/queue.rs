//! A connection's transfer queue: each upload or download you start is one
//! row, with its files copied several at a time. Rows can be paused,
//! resumed, cancelled and retried; one that a lost connection interrupted
//! continues by itself after reconnecting.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use egui::RichText;
use malasdoh_core::sftp::{Progress, human_size};
use malasdoh_core::transfer::{Ended, Job, Way};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Working out what to copy.
    Planning,
    /// Waiting for the user to choose which files it replaces.
    Reviewing,
    Running,
    /// Stopping after a pause or cancel was asked for.
    Stopping,
    Paused,
    /// Waiting for the connection to come back.
    Interrupted,
    Finished,
    Cancelled,
}

pub struct Transfer {
    pub id: u64,
    pub label: String,
    pub progress: Arc<Progress>,
    pub state: State,
    /// The files, between runs (a run takes it and gives it back).
    pub job: Option<Job>,
    /// The final summary, or why the whole transfer failed.
    pub result: Option<Result<String, String>>,
    /// Run elsewhere (a copy between servers): no pause or retry.
    external: bool,
    show_details: bool,
    speed: Speed,
}

impl Transfer {
    pub fn is_active(&self) -> bool {
        matches!(self.state, State::Planning | State::Running | State::Stopping)
    }

    /// Still has work to do (running, paused or waiting to reconnect).
    pub fn is_unfinished(&self) -> bool {
        !matches!(self.state, State::Finished | State::Cancelled)
    }

    fn failures(&self) -> usize {
        self.job.as_ref().map(|j| j.failures().len()).unwrap_or(0)
    }
}

/// Bytes per second, smoothed.
#[derive(Default)]
struct Speed {
    last: Option<(Instant, u64)>,
    rate: f64,
}

impl Speed {
    fn sample(&mut self, done: u64) -> f64 {
        let now = Instant::now();
        match self.last {
            Some((t, d)) if now.duration_since(t).as_secs_f64() >= 0.5 => {
                let dt = now.duration_since(t).as_secs_f64();
                let r = done.saturating_sub(d) as f64 / dt;
                self.rate = if self.rate == 0.0 { r } else { self.rate * 0.6 + r * 0.4 };
                self.last = Some((now, done));
            }
            Some(_) => {}
            None => self.last = Some((now, done)),
        }
        self.rate
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

fn duration(secs: f64) -> String {
    let s = secs.round() as u64;
    match s {
        0..60 => format!("{s} s"),
        60..3600 => format!("{} min {} s", s / 60, s % 60),
        _ => format!("{} h {} min", s / 3600, (s % 3600) / 60),
    }
}

/// What the user asked for on a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Pause(u64),
    /// Resume a paused row, or retry the files that failed.
    Resume(u64),
    Cancel(u64),
}

#[derive(Default)]
pub struct Queue {
    pub items: Vec<Transfer>,
    next_id: u64,
}

impl Queue {
    pub fn add(&mut self, label: String, job: Option<Job>) -> u64 {
        self.next_id += 1;
        let state = if job.is_some() { State::Running } else { State::Planning };
        self.items.push(Transfer {
            id: self.next_id,
            label,
            progress: Arc::new(Progress::default()),
            state,
            job,
            result: None,
            external: false,
            show_details: false,
            speed: Speed::default(),
        });
        self.next_id
    }

    /// A transfer run by someone else; finish it with [`Queue::finish_external`].
    pub fn add_external(&mut self, label: String) -> (u64, Arc<Progress>) {
        let id = self.add(label, None);
        let t = self.get_mut(id).unwrap();
        t.external = true;
        t.state = State::Running;
        (id, t.progress.clone())
    }

    pub fn finish_external(&mut self, id: u64, result: Result<String, String>) {
        if let Some(t) = self.get_mut(id) {
            t.state = if result.is_ok() { State::Finished } else { State::Cancelled };
            t.result = Some(result);
        }
    }

    pub fn get_mut(&mut self, id: u64) -> Option<&mut Transfer> {
        self.items.iter_mut().find(|t| t.id == id)
    }

    /// A run is starting: a fresh progress and speed.
    pub fn started(&mut self, id: u64) -> Option<(Arc<Progress>, Job)> {
        let t = self.get_mut(id)?;
        let job = t.job.take()?;
        t.progress = Arc::new(Progress::default());
        t.speed.reset();
        t.state = State::Running;
        t.result = None;
        Some((t.progress.clone(), job))
    }

    /// A run ended. `lost` says the connection went away meanwhile.
    pub fn ran(&mut self, id: u64, job: Job, ended: Result<Ended, String>, lost: bool) {
        let Some(t) = self.get_mut(id) else { return };
        t.state = match &ended {
            Ok(Ended::Paused) => State::Paused,
            Ok(Ended::Cancelled) => State::Cancelled,
            _ if lost => State::Interrupted,
            Ok(Ended::Finished) => State::Finished,
            Err(_) => State::Finished,
        };
        t.result = Some(match ended {
            Err(e) => Err(e),
            Ok(Ended::Cancelled) => Ok(format!(
                "Cancelled after {} of {} file(s); the unfinished file was removed",
                job.done(),
                job.files.len()
            )),
            Ok(_) => Ok(summary(&job)),
        });
        t.job = Some(job);
    }

    /// Waiting in "Replace files?".
    pub fn reviewing(&mut self, id: u64) {
        if let Some(t) = self.get_mut(id) {
            t.state = State::Reviewing;
        }
    }

    /// Cancelled before anything was copied.
    pub fn dropped(&mut self, id: u64) {
        if let Some(t) = self.get_mut(id) {
            t.state = State::Cancelled;
            t.result = Some(Ok("Cancelled: nothing was copied".into()));
        }
    }

    pub fn planned(&mut self, id: u64, job: Result<Job, String>) {
        let Some(t) = self.get_mut(id) else { return };
        match job {
            Ok(j) => {
                t.job = Some(j);
                t.state = State::Paused; // the session starts it right away
            }
            Err(e) => {
                t.state = State::Finished;
                t.result = Some(Err(e));
            }
        }
    }

    /// Rows a reconnect should continue.
    pub fn interrupted(&self) -> Vec<u64> {
        self.items.iter().filter(|t| t.state == State::Interrupted).map(|t| t.id).collect()
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, session: crate::bridge::SessionId) -> Vec<Control> {
        let mut out = Vec::new();
        if self.items.is_empty() {
            return out;
        }
        let running = self.items.iter().filter(|t| t.is_active()).count();
        let title = if running > 0 { format!("Transfers ({running} running)") } else { "Transfers".to_string() };
        egui::CollapsingHeader::new(title).id_salt(("queue", session)).default_open(true).show(ui, |ui| {
            for t in &mut self.items {
                row(ui, t, &mut out);
            }
            if self.items.iter().any(|t| !t.is_unfinished())
                && ui.small_button("Clear finished").on_hover_text("Remove finished and cancelled rows").clicked()
            {
                self.items.retain(Transfer::is_unfinished);
            }
        });
        if running > 0 {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
        }
        ui.separator();
        out
    }
}

/// How "Replace files?" notes a file left out for being identical.
pub const SAME: &str = "(same content, not copied again)";

fn summary(job: &Job) -> String {
    let done = job.done();
    let total = job.files.len();
    let verb = match job.way {
        Way::Up => "Uploaded",
        Way::Down => "Downloaded",
    };
    let bytes: u64 =
        job.files.iter().filter(|f| f.status == malasdoh_core::transfer::Status::Done).map(|f| f.size).sum();
    let same = job.skipped.iter().filter(|s| s.ends_with(SAME)).count();
    if total == 0 && same > 0 && same == job.skipped.len() {
        return format!("Nothing to copy: {same} file(s) already the same");
    }
    let mut s = if done == total {
        format!("{verb} {total} file(s), {}", human_size(bytes))
    } else {
        format!("{verb} {done} of {total} file(s); {} failed", total - done)
    };
    let verified = job.verified();
    if verified > 0 && verified == done {
        s.push_str(" · checksums match");
    } else if verified > 0 {
        s.push_str(&format!(" · {verified} checked by checksum"));
    }
    if same > 0 {
        s.push_str(&format!(" · {same} already the same"));
    }
    if job.skipped.len() > same {
        s.push_str(&format!(" · {} left out", job.skipped.len() - same));
    }
    s
}

fn row(ui: &mut egui::Ui, t: &mut Transfer, out: &mut Vec<Control>) {
    let p = t.progress.clone();
    let (done, total) = (p.done_bytes.load(Ordering::Relaxed), p.total_bytes.load(Ordering::Relaxed));
    let (files_done, files) = (p.done_files.load(Ordering::Relaxed), p.total_files.load(Ordering::Relaxed));
    let err = ui.visuals().error_fg_color;
    let warn = ui.visuals().warn_fg_color;
    ui.horizontal(|ui| match t.state {
        State::Planning => {
            ui.spinner();
            ui.add(egui::Label::new(format!("{} — listing files…", t.label)).truncate());
        }
        State::Reviewing => {
            ui.colored_label(warn, "waiting");
            ui.add(egui::Label::new(format!("{} — choose what it replaces (Replace files?)", t.label)).truncate());
        }
        State::Running | State::Stopping => {
            let frac = if total == 0 { 0.0 } else { done as f32 / total as f32 };
            ui.add(egui::ProgressBar::new(frac).desired_width(110.0).show_percentage());
            if !t.external {
                if ui.small_button("⏸").on_hover_text("Pause (keeps what is copied so far)").clicked() {
                    out.push(Control::Pause(t.id));
                }
                if ui.small_button("✖").on_hover_text("Cancel (removes unfinished copies)").clicked() {
                    out.push(Control::Cancel(t.id));
                }
            } else if ui.small_button("✖").on_hover_text("Cancel").clicked() {
                p.cancel.store(true, Ordering::Relaxed);
            }
            ui.add(egui::Label::new(&t.label).truncate());
        }
        State::Paused | State::Interrupted => {
            let frac = if total == 0 { 0.0 } else { done as f32 / total as f32 };
            let word = if t.state == State::Paused { "paused" } else { "waiting" };
            ui.add(
                egui::ProgressBar::new(frac).desired_width(110.0).text(format!("{word} · {}%", (frac * 100.0).round())),
            );
            if ui.small_button("▶").on_hover_text("Continue").clicked() {
                out.push(Control::Resume(t.id));
            }
            if ui.small_button("✖").on_hover_text("Cancel (removes unfinished copies)").clicked() {
                out.push(Control::Cancel(t.id));
            }
            ui.add(egui::Label::new(&t.label).truncate());
        }
        State::Finished | State::Cancelled => {
            let failed = t.failures();
            match &t.result {
                Some(Err(e)) => {
                    if t.job.is_some()
                        && !t.external
                        && ui.small_button("Retry").on_hover_text("Try this transfer again").clicked()
                    {
                        out.push(Control::Resume(t.id));
                    }
                    ui.add(egui::Label::new(RichText::new(format!("✖ {}: {e}", t.label)).color(err)).wrap());
                }
                Some(Ok(msg)) if failed > 0 => {
                    ui.colored_label(warn, "⚠");
                    if ui.small_button("Retry").on_hover_text("Copy the files that failed again").clicked() {
                        out.push(Control::Resume(t.id));
                    }
                    ui.add(egui::Label::new(msg).wrap()).on_hover_text(&t.label);
                }
                Some(Ok(msg)) => {
                    let mark = if t.state == State::Cancelled { "–" } else { "✔" };
                    ui.add(egui::Label::new(format!("{mark} {msg}")).wrap()).on_hover_text(&t.label);
                }
                None => {}
            }
        }
    });
    if t.is_active() && t.state != State::Planning {
        // Bytes already there when a partial file was continued are not speed.
        let rate = t.speed.sample(done.saturating_sub(p.resumed_bytes.load(Ordering::Relaxed)));
        let mut line = String::new();
        if files > 1 {
            line.push_str(&format!("{files_done} of {files} files · "));
        }
        line.push_str(&format!("{} of {}", human_size(done), human_size(total)));
        if rate > 1.0 {
            line.push_str(&format!(" · {}/s", human_size(rate as u64)));
            if total > done {
                line.push_str(&format!(" · {} left", duration((total - done) as f64 / rate)));
            }
        }
        if t.state == State::Stopping {
            line.push_str(" · stopping…");
        }
        let current = p.current.lock().unwrap().clone();
        ui.add(egui::Label::new(RichText::new(line).small().weak()).truncate());
        if !current.is_empty() {
            ui.add(egui::Label::new(RichText::new(current).small().weak()).truncate());
        }
    }
    if t.state == State::Interrupted {
        ui.label(RichText::new("The connection was lost; this continues after reconnecting.").small().color(warn));
    }
    let Some(job) = &t.job else { return };
    let incomplete = job.incomplete_on_server();
    if !incomplete.is_empty() && t.is_unfinished() {
        ui.label(
            RichText::new(format!(
                "{} file(s) on the server are only partly overwritten until this finishes.",
                incomplete.len()
            ))
            .small()
            .color(warn),
        )
        .on_hover_text(incomplete.join("\n"));
    }
    let failures = job.failures();
    let has_details = !failures.is_empty() || !job.skipped.is_empty() || (job.unverified.is_some() && !t.is_active());
    if !has_details || t.is_active() {
        return;
    }
    let label = if t.show_details { "Hide details" } else { "Details" };
    if ui.small_button(label).clicked() {
        t.show_details = !t.show_details;
    }
    if !t.show_details {
        return;
    }
    egui::ScrollArea::vertical().id_salt(("queue-details", t.id)).max_height(160.0).show(ui, |ui| {
        for (path, e) in &failures {
            ui.add(egui::Label::new(RichText::new(format!("✖ {path}: {e}")).small().color(err)).wrap());
        }
        if let Some(why) = &job.unverified {
            ui.label(RichText::new(format!("Not compared by checksum: {why}.")).small().weak());
        }
        for s in &job.skipped {
            ui.label(RichText::new(format!("left out: {s}")).small().weak());
        }
    });
}
