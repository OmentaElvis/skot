//! Terminal output.
//!
//! Only the two things a log record cannot express live here: the interactive progress display,
//! and the logger itself. Ordinary output is `log` at the call site -- `info!` for a result
//! the user asked for, `debug!` for supporting detail, `warn!` and `error!` for problems --
//! which means `RUST_LOG` filters by level and by module (`skot::commands::rebase=debug`)
//! with no wrapper layer in between.

use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use git2::Progress as Transfer;
use log::{Level, info};

/// Install the process logger.
///
/// Under `--json` the filter is `off` rather than merely quieter, because stdout is carrying
/// a document and any diagnostic line beside it would corrupt a parser.
pub fn install_logger(json: bool) {
    use std::io::Write as _;

    let default = if json { "off" } else { "info" };
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default));

    builder.format(|buf, record| {
        // Determine the style based strictly on the log level (maintains contextual coloring)
        let style = match record.level() {
            Level::Error => Styled::new().red().bold(),
            Level::Warn => Styled::new().yellow().bold(),
            Level::Info => Styled::new().green().bold(),
            Level::Debug => Styled::new().dim().blue(),
            Level::Trace => Styled::new().dim(),
        };

        let is_custom_target = !record.target().contains("::");

        let tag = if is_custom_target {
            record.target()
        } else {
            match record.level() {
                Level::Error => "Error",
                Level::Warn => "Warn",
                Level::Info => "Info",
                Level::Debug => "Debug",
                Level::Trace => "Trace",
            }
        };

        // {:>12} handles right-alignment automatically, no matter how long the custom tag is!
        writeln!(
            buf,
            "{:>6} {}",
            style.apply(tag),
            Styled::new().apply(record.args())
        )
    });

    let logger = builder.build();
    // `env_logger` filters per module, but `log` has only one global maximum, so the two can
    // disagree and records get dropped. Querying the built logger for its level and setting
    // the global one explicitly is the workaround the bridge documents for exactly this.
    let level = logger.filter();

    // The wrapper is what keeps records and bars from overwriting each other: it suspends the
    // display for the duration of each write.
    let installed = indicatif_log_bridge::LogWrapper::new(progress().clone(), logger).try_init();
    log::set_max_level(level);

    // A library user may have installed a logger already; theirs wins rather than panicking.
    let _ = installed;
}

/// True when we should suppress human-readable output and emit JSON instead.
static JSON: OnceLock<bool> = OnceLock::new();

pub fn set_json(on: bool) {
    let _ = JSON.set(on);
}

pub fn is_json() -> bool {
    JSON.get().copied().unwrap_or(false)
}

/// True when stderr is a terminal, i.e. colour will actually be seen.
fn colour() -> bool {
    static COLOUR: OnceLock<bool> = OnceLock::new();
    *COLOUR.get_or_init(|| {
        if is_json() {
            return false;
        }
        if std::env::var_os("NO_COLOR").is_some() {
            return false;
        }
        std::io::stderr().is_terminal()
    })
}

/// A value that renders as plain text under `--json` and as styled text otherwise.
#[derive(Clone)]
pub struct Styled {
    style: console::Style,
}

impl Styled {
    pub fn new() -> Self {
        Self {
            style: console::Style::new(),
        }
    }

    /// Supporting detail, which should recede rather than compete with the result above it.
    pub fn dim(mut self) -> Self {
        self.style = self.style.dim();
        self
    }

    /// A heading. Bold rather than a colour, so it reads as structure instead of as status.
    pub fn bold(mut self) -> Self {
        self.style = self.style.bold();
        self
    }

    /// A rule under the words, for a title that should announce where a block begins.
    pub fn underlined(mut self) -> Self {
        self.style = self.style.underlined();
        self
    }

    /// Blue highlight for detail that should still catch the eye.
    pub fn blue(mut self) -> Self {
        self.style = self.style.blue().bright();
        self
    }

    /// The ordinary case: something the command was asked to do, and did.
    ///
    /// Bright, not plain: a dim green on a dark terminal is the classic unreadable
    /// combination, and `status` is mostly this.
    pub fn green(mut self) -> Self {
        self.style = self.style.green().bright();
        self
    }

    /// A failure. Bright red, because it is the one thing on screen that needs acting on.
    pub fn red(mut self) -> Self {
        self.style = self.style.red().bright();
        self
    }

    /// Something that is not fatal but is not fine either.
    pub fn yellow(mut self) -> Self {
        self.style = self.style.yellow().bright();
        self
    }

    /// Render to a string, for building a line out of several styled parts.
    pub fn render(&self, s: impl std::fmt::Display) -> String {
        self.apply(s).to_string()
    }

    fn apply(&self, s: impl std::fmt::Display) -> console::StyledObject<impl std::fmt::Display> {
        // `apply_to` is the only form in `console` 0.16, and it is a no-op for an
        // unstyled value, so the `colour()` branch is just belt and braces for
        // terminals that set `NO_COLOR` after we decided.
        if colour() {
            self.style.apply_to(s)
        } else {
            console::Style::new().apply_to(s)
        }
    }
}

impl Default for Styled {
    fn default() -> Self {
        Self::new()
    }
}

/// The process-wide progress display, so that bars and messages cannot fight.
///
/// Everything interactive goes through this one object rather than each spinner drawing
/// itself.
fn progress() -> &'static indicatif::MultiProgress {
    static PROGRESS: OnceLock<indicatif::MultiProgress> = OnceLock::new();
    PROGRESS.get_or_init(|| {
        indicatif::MultiProgress::with_draw_target(indicatif::ProgressDrawTarget::stderr())
    })
}

/// A spinner for a long operation, which turns itself into nothing under `--json` or when
/// stderr is not a terminal, so that piped output stays clean.
pub struct Spinner {
    bar: Option<indicatif::ProgressBar>,
}

impl Spinner {
    /// A spinner with `message`, ticking until [`Spinner::finish`] is called.
    ///
    /// Whether a spinner should exist is decided from the process's own situation, never
    /// passed in as an instruction: `--json` means stdout is a document, a non-terminal
    /// stderr means nobody is watching, and a caller wanting silence asks the logger for
    /// that (the filter is `off` under `--json` anyway). Every bar joins the shared
    /// `MultiProgress`, which is built for several at once.
    pub fn new(message: &str) -> Self {
        let bar = if is_json() || !std::io::stderr().is_terminal() {
            None
        } else {
            let bar = indicatif::ProgressBar::new_spinner();
            bar.set_style(
                indicatif::ProgressStyle::with_template("  {spinner} {msg}")
                    .unwrap_or_else(|_| indicatif::ProgressStyle::default_spinner()),
            );
            bar.enable_steady_tick(std::time::Duration::from_millis(100));

            // Registered with the shared display rather than left to draw itself, so that
            // every bar and every message above are accounted for by one thing. The bridge
            // suspends *this* object around each log write, so both must be the same one.
            let bar = progress().add(bar);
            bar.set_message(message.to_owned());
            Some(bar)
        };
        Self { bar }
    }

    /// Replace the message. A no-op when there is no spinner.
    ///
    /// Deliberately not logged: libgit2 calls this on every sideband update, so a record here
    /// would fire dozens of times a second and each one suspends and redraws the display.
    pub fn set_message(&self, message: &str) {
        if let Some(bar) = &self.bar {
            bar.set_message(message.to_owned());
        }
    }

    /// Show `step` units done out of `max`, if the total is known.
    fn set_position(&self, step: u64, max: Option<u64>) {
        if let Some(bar) = &self.bar {
            bar.set_length(max.unwrap_or(0));
            bar.set_position(step);
        }
    }

    /// Retire the spinner.
    ///
    /// This only unregisters the bar. It must not call `finish_and_clear` on it: that clears
    /// straight to the terminal, bypassing the `MultiProgress`'s bookkeeping about where its
    /// lines are, and `remove` then clears a second time, landing on whatever has since
    /// moved into that space. The result is a stale, non-animating bar left on screen and
    /// unrelated lines erased. `MultiProgress::remove` does the one correct clear.
    ///
    /// Idempotent, so calling it here and again from `drop` is harmless.
    pub fn finish(&self) {
        if let Some(bar) = &self.bar {
            progress().remove(bar);
        }
    }
}

impl Drop for Spinner {
    /// Unregister the bar, whatever happened.
    ///
    /// This cannot be left to [`Spinner::finish`]. A fetch that fails returns early through
    /// `?`, so `finish` never runs, and a bar left registered keeps its steady-tick thread
    /// alive and keeps redrawing over whatever is written below it.
    fn drop(&mut self) {
        if let Some(bar) = &self.bar {
            progress().remove(bar);
        }
    }
}

/// A spinner fed by a `git2` fetch.
///
/// libgit2 reports progress through two channels rather than one tree of named units:
/// `sideband_progress` carries the remote's own lines ("Counting objects: 100% (2443/2443),
/// done."), and `transfer_progress` carries object and delta counts plus bytes received.
/// Both are wired up by [`Progress::attach_to`].
///
/// The remote's line is worth showing while it is speaking, it names the phase
/// ("Counting", "Compressing") that a bare counter cannot, and it is what git shows. A
/// chunk can carry a whole phase at once, so only its last non-empty line is taken: that
/// is the current one, and a multi-line message would not fit the one-line spinner. A
/// line declaring its phase done is logged instead, so it stays in the console
/// Our own counter takes the line only in the remote's silences, the pack
/// streaming and the delta resolution that follow, where otherwise the display would sit
/// still.
///
/// The callbacks are called from the transport's thread, so the counters are atomics and
/// the spinner is shared.
///
/// The alternative is no callbacks at all, which is correct and silent. A mirror of a real
/// project is minutes of network, and silence that long looks exactly like a hang.
pub struct Progress {
    spinner: Arc<Spinner>,
    /// Objects received so far. libgit2 reports this as a whole counter rather than an
    /// increment, so it is stored to keep the last value visible on the spinner.
    received: AtomicUsize,
    /// The pack's total object count.
    total: AtomicUsize,
    /// False while `total` is still 0, so "0 objects" is not rendered as a finished bar.
    bounded: AtomicBool,
    /// Deltas indexed so far, the progress of the quiet stretch after the transfer.
    indexed_deltas: AtomicUsize,
    /// The pack's total delta count.
    total_deltas: AtomicUsize,
    /// When the remote last spoke, so our counter can yield while its line is current.
    sideband_at: Mutex<Option<Instant>>,
}

impl Progress {
    pub fn new(message: &str) -> Self {
        Self {
            spinner: Arc::new(Spinner::new(message)),
            received: AtomicUsize::new(0),
            total: AtomicUsize::new(0),
            bounded: AtomicBool::new(false),
            indexed_deltas: AtomicUsize::new(0),
            total_deltas: AtomicUsize::new(0),
            sideband_at: Mutex::new(None),
        }
    }

    /// Feed this spinner from a set of remote callbacks.
    pub fn attach_to<'a>(&'a self, callbacks: &mut git2::RemoteCallbacks<'a>) {
        // Two closures over the same `&self`, which is why the counters are atomics and
        // the spinner is behind an `Arc`.
        let on_transfer = |stats: Transfer<'_>| {
            let total = stats.total_objects();
            if total > 0 {
                self.total.store(total, Ordering::Relaxed);
                self.bounded.store(true, Ordering::Relaxed);
            }
            let received = stats.received_objects();
            self.received.store(received, Ordering::Relaxed);
            self.indexed_deltas
                .store(stats.indexed_deltas(), Ordering::Relaxed);
            self.total_deltas
                .store(stats.total_deltas(), Ordering::Relaxed);
            // The remote's own line stays current while it is speaking; our counter only
            // takes over once it has been quiet, so the two never fight over the line.
            if self.remote_is_quiet() {
                self.spinner
                    .set_message(&self.status_line(stats.received_bytes()));
            }
            self.spinner
                .set_position(received as u64, self.bound().map(|t| t as u64));
            // Always continue: returning false cancels the fetch, and a progress display
            // is not a reason to abort work the user asked for.
            true
        };
        callbacks.transfer_progress(on_transfer);

        let on_sideband = |message: &[u8]| {
            self.observe(&String::from_utf8_lossy(message));
            true
        };
        callbacks.sideband_progress(on_sideband);
    }

    /// Take a chunk of the remote's sideband chatter and put the current line on screen.
    ///
    /// A chunk may carry a whole phase's worth of lines; only the last non-empty one is
    /// current, and a multi-line message would not fit the spinner. A line that declares
    /// its phase done is history rather than state: it is logged, so it stays in the
    /// console the way git leaves it, and the spinner moves on with the next phase.
    fn observe(&self, text: &str) {
        let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
        let Some(latest) = lines.next_back() else {
            return;
        };
        *self.sideband_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        if latest.contains("done") {
            info!(target: "git", "{latest}");
        } else {
            self.spinner.set_message(latest);
        }
    }

    /// True once the remote has been silent long enough for our counter to take the line.
    fn remote_is_quiet(&self) -> bool {
        self.sideband_at
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none_or(|at| at.elapsed() >= Duration::from_millis(500))
    }

    /// What the spinner says while the remote says nothing itself.
    ///
    /// The phases in the order a fetch spends its time: receiving objects against a known
    /// total, then resolving deltas — where the object counters stop moving and a static
    /// line would read as a hang.
    fn status_line(&self, bytes: usize) -> String {
        let received = self.received.load(Ordering::Relaxed);
        let total = self.total.load(Ordering::Relaxed);
        let size = crate::layout::human_bytes(bytes as u64);
        if total > 0 && received >= total {
            let done = self.indexed_deltas.load(Ordering::Relaxed);
            let all = self.total_deltas.load(Ordering::Relaxed);
            if all > 0 && done < all {
                return format!("resolving deltas {done}/{all}");
            }
            return format!("received {received} objects, {size}");
        }
        // `checked_div` also covers the uncounted case: with no total there is no
        // percentage, and the line says what is known rather than dividing by zero.
        match received
            .checked_mul(100)
            .and_then(|pct| pct.checked_div(total))
        {
            Some(pct) => format!("receiving {received}/{total} objects ({pct}%), {size}"),
            None => format!("receiving {received} objects, {size}"),
        }
    }

    /// The upper bound on objects, or `None` while the pack is still being counted.
    fn bound(&self) -> Option<usize> {
        self.bounded
            .load(Ordering::Relaxed)
            .then(|| self.total.load(Ordering::Relaxed))
    }

    /// Stop the spinner and clear the line.
    pub fn finish(&self) {
        self.spinner.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_json_flag_is_a_process_wide_once() {
        // `set_json` is a OnceLock, so this test owns the process-wide value. It is the one
        // test in the crate that may run first or last; ordering between test binaries is not
        // an issue because each is its own process.
        set_json(true);
        assert!(is_json());
        // Only the progress display consults it now. Suppressing ordinary output is the
        // logger's job, via the `off` filter that `install_logger` installs under `--json`.
        assert!(Spinner::new("fetching").bar.is_none());
    }
}

#[cfg(test)]
mod progress_tests {
    use super::*;

    /// `set_json` is a process-wide `OnceLock`, so the progress tests must not care about
    /// a spinner existing. There is no terminal in `cargo test`, so these only check the
    /// bookkeeping, which is the part with logic in it.
    #[test]
    fn an_unknown_total_is_not_reported_as_zero() {
        let p = Progress::new("fetching");
        // This is the state before libgit2 has counted the pack, and it must not render
        // as a finished 0/0 bar.
        assert_eq!(p.bound(), None);
    }

    #[test]
    fn the_spinner_never_lays_a_claim_on_a_fetch() {
        // The callbacks take `&self` and are invoked from the transport thread. If
        // attaching needed `&mut self` this would not compile, which is the point of the
        // test: it pins the shared-borrow shape that makes the fetch possible at all.
        let p = Progress::new("fetching");
        let mut callbacks = git2::RemoteCallbacks::new();
        p.attach_to(&mut callbacks);
        let mut again = git2::RemoteCallbacks::new();
        p.attach_to(&mut again);
        p.finish();
    }

    #[test]
    fn a_chunk_gives_up_only_its_last_live_line() {
        let p = Progress::new("fetching");
        // A server may flush a whole phase in one chunk. The spinner must remember when
        // the remote spoke without trying to render the rest of the chunk into a
        // one-line spinner.
        p.observe("Counting objects: 54% (1319/2443)\nCounting objects: 100% (2443/2443)\n");
        assert!(p.sideband_at.lock().unwrap().is_some());
        assert!(!p.remote_is_quiet());

        // Whitespace-only flushes are not speech and must not silence our own counter.
        let silent = Progress::new("fetching");
        silent.observe("\n");
        assert!(silent.remote_is_quiet());
    }

    #[test]
    fn the_own_counter_names_the_phase_it_is_in() {
        let p = Progress::new("fetching");
        // Before the pack is counted there is no total, and the line must not pretend
        // otherwise.
        assert_eq!(p.status_line(0), "receiving 0 objects, 0B");
        p.total.store(100, Ordering::Relaxed);
        p.bounded.store(true, Ordering::Relaxed);
        p.received.store(50, Ordering::Relaxed);
        assert_eq!(p.status_line(1024), "receiving 50/100 objects (50%), 1.0K");
        // Once the objects are in, the object counters stop moving; deltas take over so
        // the line does not sit still for the quietest part of the fetch.
        p.received.store(100, Ordering::Relaxed);
        p.total_deltas.store(10, Ordering::Relaxed);
        assert_eq!(p.status_line(1024), "resolving deltas 0/10");
        p.indexed_deltas.store(10, Ordering::Relaxed);
        assert_eq!(p.status_line(1024), "received 100 objects, 1.0K");
    }
}
