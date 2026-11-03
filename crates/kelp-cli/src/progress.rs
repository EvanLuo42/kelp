//! Progress bars for long workspace operations.

use std::io::IsTerminal;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use indicatif::{HumanBytes, ProgressBar, ProgressDrawTarget, ProgressStyle};
use kelp_client::progress::{Phase, Progress, Update};
use kelp_client::{ContentCache, Home};

/// Draws a bar for the current phase on standard error, if it is a terminal.
pub struct Bars {
    enabled: bool,
    current: Mutex<Option<(Phase, ProgressBar)>>,
}

impl Bars {
    pub fn new() -> Self {
        Self {
            enabled: std::io::stderr().is_terminal(),
            current: Mutex::new(None),
        }
    }

    /// Removes the bar, if any.
    pub fn clear(&self) {
        if let Some((_, bar)) = self.current.lock().expect("progress lock").take() {
            bar.finish_and_clear();
        }
    }
}

impl Progress for Bars {
    fn update(&self, update: Update) {
        if !self.enabled {
            return;
        }
        let mut current = self.current.lock().expect("progress lock");
        if current.as_ref().is_none_or(|(p, _)| *p != update.phase) {
            if let Some((_, old)) = current.take() {
                old.finish_and_clear();
            }
            *current = Some((update.phase, bar(update.phase, update.total_items)));
        }
        let (_, bar) = current.as_ref().expect("bar created");
        bar.set_length(update.total_items);
        bar.set_position(update.items);
        bar.set_message(match update.total_bytes {
            Some(total) => format!("{} / {}", HumanBytes(update.bytes), HumanBytes(total)),
            None => HumanBytes(update.bytes).to_string(),
        });
        if update.is_finished() {
            bar.finish_and_clear();
            *current = None;
        }
    }
}

impl Drop for Bars {
    fn drop(&mut self) {
        self.clear();
    }
}

/// Returns the user's content cache, drawing bars for its transfers until
/// the last clone of it is dropped.
pub fn cache(home: &Home) -> ContentCache {
    home.cache().with_progress(Arc::new(Bars::new()))
}

fn bar(phase: Phase, total: u64) -> ProgressBar {
    let unit = match phase {
        Phase::Download | Phase::Upload => "chunks",
        Phase::Scan | Phase::Write | Phase::Read => "files",
        Phase::Wait => "",
    };
    let template = match phase {
        Phase::Wait => {
            "{prefix:>11} for another Kelp command in this workspace to finish  {elapsed}".into()
        }
        _ => format!(
            "{{prefix:>11}} [{{bar:30}}] {{human_pos}}/{{human_len}} {unit}  {{msg}}  {{elapsed}}"
        ),
    };
    let style = ProgressStyle::with_template(&template)
        .expect("valid template")
        .progress_chars("=> ");
    let bar = ProgressBar::with_draw_target(Some(total), ProgressDrawTarget::stderr())
        .with_style(style)
        .with_prefix(phase.name());
    bar.enable_steady_tick(Duration::from_millis(200));
    bar
}
