//! What to do next, after a command succeeded.
//!
//! Advice used to be `info!` lines, which made it subject to the log filter and gave it no
//! structure: a tip vanished under `RUST_LOG=warn` and ran together with the narration it
//! followed. [`Hints`] builds one block of advice — a heading, then rows — and prints it on
//! stderr beside that narration, with its own switch: `--no-hints` drops it from any
//! command, and `--json` never has it because a document answers instead of advice.
//!
//! Failure remedies deliberately stay in the log. A flag that drops tips must not also drop
//! the sentence telling you how to fix a command that went wrong.
//!
//! ```
//! use skot::hints::Hints;
//! use skot::invocation::skot;
//!
//! Hints::new("next")
//!     .entry(skot("check"), "re-verify the whole series")
//!     .entry(skot("push"), "apply the next patch")
//!     .print();
//! ```

use std::sync::atomic::{AtomicBool, Ordering};

use crate::ui::Styled;

/// Width of the command column: wide enough for `skot patch edit-save <name>`, the longest
/// entry today, so every description starts in the same place and the block reads as pairs
/// rather than as prose.
const CMD: usize = 25;

static ENABLED: AtomicBool = AtomicBool::new(true);

/// Turn hints on or off for the process. `main` calls this once from `--no-hints` before
/// any command runs.
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// Whether hints print at all. `--json` is checked here as well as at every `--json` early
/// return, so a command that forgets its own cannot put advice in a document's stream.
fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed) && !crate::ui::is_json()
}

/// A block of advice, built up and printed in one go.
pub struct Hints {
    title: String,
    rows: Vec<Row>,
}

enum Row {
    /// A `command   purpose` pair, padded into the column.
    Entry { command: String, purpose: String },
    /// Free prose, for instructions with no command to run.
    Line(String),
}

impl Hints {
    /// A block headed `title`.
    pub fn new(title: impl Into<String>) -> Self {
        Hints {
            title: title.into(),
            rows: Vec::new(),
        }
    }

    /// Add a `command   purpose` row. The command is bold because it is the part to act on.
    pub fn entry(mut self, command: impl Into<String>, purpose: impl Into<String>) -> Self {
        self.rows.push(Row::Entry {
            command: command.into(),
            purpose: purpose.into(),
        });
        self
    }

    /// Add a line of prose under the heading.
    pub fn line(mut self, text: impl Into<String>) -> Self {
        self.rows.push(Row::Line(text.into()));
        self
    }

    /// Print the block to stderr, unless hints are off.
    pub fn print(self) {
        if !enabled() {
            return;
        }
        eprint!("{}", self.render());
    }

    /// The block exactly as [`print`] would write it.
    fn render(&self) -> String {
        let bold = Styled::new().bold();
        let title = Styled::new().bold().underlined().render(&self.title);
        let mut out = format!("\n{title}\n");
        for row in &self.rows {
            match row {
                Row::Entry { command, purpose } => {
                    out.push_str(&format!(
                        "  {}  {purpose}\n",
                        bold.render(format!("{command:<CMD$}"))
                    ));
                }
                Row::Line(text) => out.push_str(&format!("  {text}\n")),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `enabled()` also reads `--json`, which other tests may have set process-wide, so
    /// this asserts the flag itself rather than the combined predicate.
    #[test]
    fn no_hints_switches_the_flag_off_and_back_on() {
        set_enabled(false);
        assert!(!ENABLED.load(Ordering::Relaxed));
        set_enabled(true);
        assert!(ENABLED.load(Ordering::Relaxed));
    }

    /// The expected side styles itself with the same `Styled`, so the assertion holds on a
    /// terminal (where colour is on) as well as in a pipe, while the literal padding still
    /// pins the column.
    #[test]
    fn a_block_is_a_heading_then_rows_with_the_command_padded_to_the_column() {
        let command_line = crate::invocation::skot("check");
        let out = Hints::new("next")
            .entry(command_line.as_str(), "re-verify the series")
            .line("edit the config")
            .render();

        let heading = Styled::new().bold().underlined().render("next");
        let command = Styled::new().bold().render(format!("{command_line:<CMD$}"));
        assert_eq!(
            out,
            format!("\n{heading}\n  {command}  re-verify the series\n  edit the config\n")
        );
    }
}
