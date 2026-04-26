//! Terminal output helpers for progress indicators and Cargo-style status lines.

use std::env;
use std::io::IsTerminal;
use std::time::Duration;

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};

const STATUS_WIDTH: usize = 12;
const ANSI_BOLD_CYAN: &str = "\x1b[1;36m";
const ANSI_BOLD_GREEN: &str = "\x1b[1;32m";
const ANSI_BOLD_RED: &str = "\x1b[1;31m";
const ANSI_BOLD_YELLOW: &str = "\x1b[1;33m";
const ANSI_RESET: &str = "\x1b[0m";

/// Status categories used to render final lockfile change summaries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum StatusKind {
    Adding,
    Updating,
    Downgrading,
    Removing,
    Keeping,
    Finished,
    Warning,
}

impl StatusKind {
    fn label(self) -> &'static str {
        match self {
            Self::Adding => "Adding",
            Self::Updating => "Updating",
            Self::Downgrading => "Downgrading",
            Self::Removing => "Removing",
            Self::Keeping => "Keeping",
            Self::Finished => "Finished",
            Self::Warning => "Warning",
        }
    }

    fn color_code(self) -> &'static str {
        match self {
            Self::Adding => ANSI_BOLD_CYAN,
            Self::Removing => ANSI_BOLD_RED,
            Self::Keeping | Self::Warning => ANSI_BOLD_YELLOW,
            Self::Updating | Self::Downgrading | Self::Finished => ANSI_BOLD_GREEN,
        }
    }
}

/// Long-lived UI handle for the cooldown resolver phase.
///
/// The executor creates this after configuration is loaded and uses it while the
/// dependency graph is being inspected and cooled. It owns terminal capability
/// decisions so resolver code can report phases and counts without knowing
/// whether output is interactive, colored, or quiet.
pub struct UserOutput {
    progress: Option<ProgressBar>,
    progress_total: usize,
    use_color: bool,
}

impl UserOutput {
    /// Create progress output according to terminal and verbosity settings.
    ///
    /// Verbose mode disables spinners so debug logs remain readable. Non-TTY
    /// output also disables progress animation and leaves only stable status
    /// lines for scripts or CI logs.
    pub fn new(verbose: bool) -> Self {
        let interactive = std::io::stderr().is_terminal();
        let use_color = colors_enabled(interactive);
        let progress = progress_enabled(interactive, verbose).then(|| {
            let progress = ProgressBar::new_spinner();
            progress.set_draw_target(ProgressDrawTarget::stderr_with_hz(10));
            progress.set_style(phase_style(use_color));
            progress.enable_steady_tick(Duration::from_millis(120));
            progress
        });

        Self {
            progress,
            progress_total: 0,
            use_color,
        }
    }

    /// Show a phase message before a numeric progress total is known.
    pub fn set_phase(&self, message: &str) {
        if let Some(progress) = &self.progress {
            progress.set_style(phase_style(self.use_color));
            progress.set_message(message.to_string());
        }
    }

    /// Update the resolver progress bar from the latest scan summary.
    ///
    /// `unresolved_fresh` is the active work remaining. The total is kept as the
    /// maximum seen in this run so later passes can move the bar forward without
    /// shrinking it when Cargo's graph changes.
    pub fn update_resolver_progress(
        &mut self,
        pass: usize,
        registry_packages: usize,
        inspected: usize,
        unresolved_fresh: usize,
    ) {
        let Some(progress) = &self.progress else {
            return;
        };

        self.progress_total = self.progress_total.max(unresolved_fresh);
        if self.progress_total == 0 {
            progress.finish_and_clear();
            return;
        }

        progress.set_style(progress_style(self.use_color));
        progress.set_length(self.progress_total as u64);
        progress.set_position(self.progress_total.saturating_sub(unresolved_fresh) as u64);
        progress.set_message(format!(
            "cooldown pass {pass} ({unresolved_fresh} fresh remaining, inspected {inspected}/{registry_packages})"
        ));
    }

    pub fn finish_progress(&self) {
        if let Some(progress) = &self.progress {
            progress.finish_and_clear();
        }
    }

    pub fn use_color(&self) -> bool {
        self.use_color
    }
}

/// Short-lived spinner used before a full progress total is known.
///
/// This is used around pre-scan work such as capturing the baseline or running
/// the initial `cargo update`. It clears itself on drop so early returns do not
/// leave stale spinner lines in the terminal.
pub struct PhaseStatus {
    progress: Option<ProgressBar>,
    use_color: bool,
}

impl PhaseStatus {
    pub fn new(verbose: bool) -> Self {
        let interactive = std::io::stderr().is_terminal();
        let use_color = colors_enabled(interactive);
        let progress = progress_enabled(interactive, verbose).then(|| {
            let progress = ProgressBar::new_spinner();
            progress.set_draw_target(ProgressDrawTarget::stderr_with_hz(10));
            progress.set_style(phase_style(use_color));
            progress.enable_steady_tick(Duration::from_millis(120));
            progress
        });

        Self {
            progress,
            use_color,
        }
    }

    pub fn set_message(&self, message: &str) {
        if let Some(progress) = &self.progress {
            progress.set_style(phase_style(self.use_color));
            progress.set_message(message.to_string());
        }
    }

    pub fn finish(&self) {
        if let Some(progress) = &self.progress {
            progress.finish_and_clear();
        }
    }
}

impl Drop for PhaseStatus {
    fn drop(&mut self) {
        self.finish();
    }
}

pub fn format_status_line(kind: StatusKind, message: &str, use_color: bool) -> String {
    let label = format!("{:>width$}", kind.label(), width = STATUS_WIDTH);
    if !use_color {
        return format!("{label} {message}");
    }

    format!("{}{}{} {}", kind.color_code(), label, ANSI_RESET, message)
}

fn colors_enabled(interactive: bool) -> bool {
    if env::var_os("NO_COLOR").is_some() {
        return false;
    }

    match env::var("CARGO_TERM_COLOR").ok().as_deref() {
        Some("always") => true,
        Some("never") => false,
        _ => interactive,
    }
}

fn progress_enabled(interactive: bool, verbose: bool) -> bool {
    if !interactive || verbose {
        return false;
    }

    !matches!(
        env::var("CARGO_TERM_PROGRESS_WHEN").ok().as_deref(),
        Some("never")
    )
}

fn progress_style(use_color: bool) -> ProgressStyle {
    let template = if use_color {
        "{spinner:.green} [{wide_bar:.cyan/blue}] {pos}/{len} {msg}"
    } else {
        "{spinner} [{wide_bar}] {pos}/{len} {msg}"
    };

    ProgressStyle::with_template(template)
        .expect("progress template should be valid")
        .progress_chars("=>-")
}

fn phase_style(use_color: bool) -> ProgressStyle {
    let template = if use_color {
        "{spinner:.green} {msg}"
    } else {
        "{spinner} {msg}"
    };

    ProgressStyle::with_template(template).expect("phase progress template should be valid")
}

/// Unit tests for stable user-facing status formatting.
#[cfg(test)]
mod tests {
    use super::{StatusKind, format_status_line};

    #[test]
    fn format_status_line_aligns_without_color() {
        let line = format_status_line(StatusKind::Updating, "serde 1.0.0 -> 1.0.1", false);
        assert_eq!(line, "    Updating serde 1.0.0 -> 1.0.1");
    }
}
