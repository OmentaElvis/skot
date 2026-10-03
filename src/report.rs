//! Presentation for output a command is *asked for*.
//!
//! `status`, `list` and `check` exist to be read, and sometimes piped. What they produce is
//! the answer to the question, not a note about how the answer was reached, so it is not a log
//! record: it goes to stdout, it is not filtered by `RUST_LOG`, and `--json` is its
//! machine-readable form rather than a replacement for it. Routing it through `log` would mean
//! `RUST_LOG=warn` silences the output a user explicitly asked for.
//!
//! Diagnostics are the other thing entirely — they go through `log` to stderr and appear in
//! `git/mirror.rs` and friends. The split is: what the command produced goes here, what it did
//! on the way there is a log, and what to do next is neither — that is [`crate::hints`],
//! advice on stderr with its own switch rather than the log filter's.

use crate::ui::Styled;

/// Width of the key column in [`field`]. Long enough for `sandbox policy`, short enough that
/// values start in the same place on every line, which is what makes a block scannable.
const KEY: usize = 14;

/// A heading, starting a new block.
pub fn section(title: &str) {
    println!("\n{}", Styled::new().bold().render(title));
}

/// A `key   value` line. The key is dim so the values read as the content.
pub fn field(key: &str, value: &str) {
    println!(
        "  {}  {}",
        Styled::new().dim().render(format!("{key:<KEY$}")),
        value
    );
}

/// One item in a list, already formatted by the caller.
pub fn item(text: &str) {
    println!("  {text}");
}

/// Supporting prose that belongs with the report rather than in the log.
pub fn note(text: &str) {
    println!("{}", Styled::new().dim().render(text));
}

/// A line of the report in its entirety.
pub fn line(text: &str) {
    println!("{text}");
}

/// A healthy value.
pub fn ok(text: &str) -> String {
    Styled::new().green().render(text)
}

/// A value that is wrong and needs fixing.
pub fn bad(text: &str) -> String {
    Styled::new().red().render(text)
}

/// A value that is not wrong, but is not fine.
pub fn warn(text: &str) -> String {
    Styled::new().yellow().render(text)
}

/// Secondary text that is present but not the point: paths under a name, hints after a result.
pub fn muted(text: &str) -> String {
    Styled::new().dim().render(text)
}

/// A value that is simply not there. Dim, because absence is not a problem.
pub fn absent(text: &str) -> String {
    Styled::new().dim().render(text)
}
