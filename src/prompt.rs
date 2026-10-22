//! Asking, and the one place that knows whether anybody is there.
//!
//! Every question a command cannot answer from its flags goes through [`Asker`], so
//! "what happens when nobody can answer" is decided once instead of per command, the
//! same way `indicatif` turns its progress bars off when there is no terminal to show
//! them to. The signals are the invocation itself: `--json` and `--yes` both mean
//! nobody is waiting at the prompt, and a terminal on stdin decides the rest. This
//! module owns every `inquire` call in the crate; a command passes the question, the
//! help line and the fallback, never the prompt machinery.
//!
//! One rule covers every question, and it is the rule of pressing Enter:
//!
//! - A question with a default takes the default when it cannot be asked
//!   ([`Fallback::Default`]), and arrives preselected when it can.
//! - A question only a flag can answer is refused, and `main` renders that refusal
//!   the way clap renders a missing flag: the message, the command's usage line, and
//!   exit code 2 ([`Fallback::Flag`]). Under `--yes` the yes/no form of that question
//!   is answered yes, which is all `--yes` ever does: it never invents a value it
//!   cannot know.
//!
//! Under the test harness there is never a terminal, so tests exercise the silent
//! branch and cannot block on a person.

/// True when stdin is a terminal and a prompt would have somebody to answer it.
///
/// Always false in tests: `cargo test` run interactively has a terminal on stdin, and
/// an `inquire` prompt opened against it would sit there waiting for a person.
pub fn stdin_is_terminal() -> bool {
    #[cfg(test)]
    {
        false
    }
    #[cfg(not(test))]
    {
        use std::io::IsTerminal as _;
        std::io::stdin().is_terminal()
    }
}

/// What a run answers a question it cannot ask: what Enter would answer, or, when
/// only a flag can answer it, the flag worded as advice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fallback<T> {
    /// The answer pressing Enter gives, and the answer a silent run takes.
    Default(T),
    /// The question has no default and only a flag answers it: the flag, worded as
    /// advice, so the refusal can name the way out.
    Flag(String),
}

/// Why a question stopped: it was not answered, or it was answered and the answer
/// was not yes.
#[derive(Debug, thiserror::Error)]
pub enum PromptError {
    /// Esc at the prompt. The person answering decided to stop, so the run stops;
    /// the init hook wizard catches this and finishes with what has been set.
    #[error("cancelled")]
    Cancelled,
    /// Ctrl+C at the prompt: the same stop, from the keyboard rather than Esc.
    #[error("interrupted")]
    Interrupted,
    /// The question is real and nobody is there to answer it. `main` finds this in
    /// the error chain and prints it as clap would print a missing flag.
    #[error("cannot ask {question}; answer it with: {instead}")]
    NoAnswer { question: String, instead: String },
    /// inquire failed for a mechanical reason of its own: its IO, a broken
    /// configuration. A malfunction, so it is reported as one.
    #[error(transparent)]
    Failed(#[from] inquire::InquireError),
}

/// Who is there to answer this run's questions, and how a question they cannot
/// answer is answered.
///
/// Built from the invocation: `--json` and `--yes` both mean nobody is waiting, even
/// on a terminal. `with_yes` folds in a command that carries its own `--yes`
/// (`series undo`) rather than reading the global one.
#[derive(Debug, Clone, Copy)]
pub struct Asker {
    interactive: bool,
    yes: bool,
}

impl Asker {
    /// The picture for one run, read from the invocation itself: `--json` and `--yes`
    /// both mean nobody is waiting, and the terminal on stdin decides the rest.
    pub fn for_run(json: bool, yes: bool) -> Self {
        Self::new(json, yes, stdin_is_terminal())
    }

    /// The picture for one run: prompts need a terminal and no flag that says
    /// nobody is there.
    pub fn new(json: bool, yes: bool, stdin_is_terminal: bool) -> Self {
        Self {
            interactive: !json && !yes && stdin_is_terminal,
            yes,
        }
    }

    /// Fold a command-local `--yes` into the picture: it silences every question
    /// of the run, the same way the global one does.
    pub fn with_yes(self, yes: bool) -> Self {
        if yes {
            Self {
                interactive: false,
                yes: true,
            }
        } else {
            self
        }
    }

    /// Whether anyone is there to answer: `--json`, `--yes`, and a missing terminal
    /// all mean no. A site that takes a default on a silent run reports it; on an
    /// interactive one the question just showed the same default on screen.
    pub fn is_interactive(&self) -> bool {
        self.interactive
    }

    /// A line of text. Enter answers empty, so a silent run answers empty too and
    /// the site maps that to its default the same way it maps an empty line. `esc`
    /// names what Esc answers here (the wizard's value prompt goes back), so the line
    /// left on screen does not call a decision a cancellation.
    pub fn text(
        &self,
        message: &str,
        help: Option<&str>,
        placeholder: Option<&str>,
        fallback: Fallback<String>,
        esc: Option<&'static str>,
    ) -> Result<String, PromptError> {
        self.ask(message, fallback, || {
            let mut prompt = inquire::Text::new(message);
            if let Some(placeholder) = placeholder {
                prompt = prompt.with_placeholder(placeholder);
            }
            if let Some(esc) = esc {
                prompt = prompt.with_render_config(
                    inquire::ui::RenderConfig::default_colored()
                        .with_canceled_prompt_indicator(inquire::ui::Styled::new(esc)),
                );
            }
            if let Some(help) = help {
                prompt = prompt.with_help_message(help);
            }
            prompt.prompt()
        })
    }

    /// A line of text no other answer will do: `accepts` decides what is well-formed,
    /// at the site that knows what a valid answer is (a position, a name, a number).
    pub fn text_validated(
        &self,
        message: &str,
        help: Option<&str>,
        fallback: Fallback<String>,
        accepts: impl Fn(&str) -> std::result::Result<(), String> + Clone + 'static,
    ) -> Result<String, PromptError> {
        self.ask(message, fallback, || {
            let mut prompt = inquire::Text::new(message);
            if let Some(help) = help {
                prompt = prompt.with_help_message(help);
            }
            prompt
                .with_validator(move |input: &str| {
                    Ok(match accepts(input) {
                        Ok(()) => inquire::validator::Validation::Valid,
                        Err(message) => inquire::validator::Validation::Invalid(message.into()),
                    })
                })
                .prompt()
        })
    }

    /// One line of a list. The cursor starts on what Enter takes, so a question whose
    /// default is one of the options opens on it. `page_size` shows every option at once
    /// when the whole list must be visible without scrolling (the hook wizard's exit).
    /// `esc` names what Esc answers on this prompt: some prompts answer with it (the
    /// wizard's menu finishes) instead of abandoning the run, and the line left on
    /// screen must say that, not `<canceled>`.
    pub fn select<S: Into<String>>(
        &self,
        message: &str,
        help: Option<&str>,
        options: Vec<S>,
        fallback: Fallback<String>,
        page_size: Option<usize>,
        esc: Option<&'static str>,
    ) -> Result<String, PromptError> {
        let options: Vec<String> = options.into_iter().map(Into::into).collect();
        let start = match &fallback {
            Fallback::Default(answer) => options.iter().position(|o| o == answer).unwrap_or(0),
            Fallback::Flag(_) => 0,
        };
        self.ask(message, fallback, || {
            let mut prompt = inquire::Select::new(message, options).with_starting_cursor(start);
            if let Some(page_size) = page_size {
                prompt = prompt.with_page_size(page_size);
            }
            if let Some(esc) = esc {
                prompt = prompt.with_render_config(
                    inquire::ui::RenderConfig::default_colored()
                        .with_canceled_prompt_indicator(inquire::ui::Styled::new(esc)),
                );
            }
            if let Some(help) = help {
                prompt = prompt.with_help_message(help);
            }
            prompt.prompt()
        })
    }

    /// Several lines of a list, with what the fallback keeps already selected, so a
    /// question whose default is "all of them" arrives with all of them held and
    /// Enter confirms the default.
    pub fn multi(
        &self,
        message: &str,
        help: Option<&str>,
        options: Vec<String>,
        fallback: Fallback<Vec<String>>,
    ) -> Result<Vec<String>, PromptError> {
        let preselect: Vec<usize> = match &fallback {
            Fallback::Default(keep) => keep
                .iter()
                .filter_map(|value| options.iter().position(|o| o == value))
                .collect(),
            Fallback::Flag(_) => Vec::new(),
        };
        self.ask(message, fallback, || {
            let mut prompt = inquire::MultiSelect::new(message, options);
            if !preselect.is_empty() {
                prompt = prompt.with_default(&preselect);
            }
            if let Some(help) = help {
                prompt = prompt.with_help_message(help);
            }
            prompt.prompt()
        })
    }

    /// A yes/no question. Enter says the default; a question only `--yes` can answer
    /// is answered yes under `--yes` and refused otherwise.
    pub fn confirm(
        &self,
        message: &str,
        help: Option<&str>,
        fallback: Fallback<bool>,
    ) -> Result<bool, PromptError> {
        let default = matches!(&fallback, Fallback::Default(true));
        if !self.interactive {
            return match fallback {
                Fallback::Default(answer) => Ok(answer),
                Fallback::Flag(_) if self.yes => Ok(true),
                Fallback::Flag(instead) => Err(PromptError::NoAnswer {
                    question: message.to_owned(),
                    instead,
                }),
            };
        }
        self.ask(message, fallback, || {
            let mut prompt = inquire::Confirm::new(message).with_default(default);
            if let Some(help) = help {
                prompt = prompt.with_help_message(help);
            }
            prompt.prompt()
        })
    }

    /// The one mechanism: open the prompt when somebody is there, answer from the
    /// fallback when nobody is, and name the flag when only a flag can answer.
    fn ask<T>(
        &self,
        question: &str,
        fallback: Fallback<T>,
        build: impl FnOnce() -> Result<T, inquire::InquireError>,
    ) -> Result<T, PromptError> {
        if self.interactive {
            return build().map_err(from_inquire);
        }
        match fallback {
            Fallback::Default(answer) => Ok(answer),
            Fallback::Flag(instead) => Err(PromptError::NoAnswer {
                question: question.to_owned(),
                instead,
            }),
        }
    }
}

/// Esc and Ctrl+C are decisions, not failures of the terminal.
fn from_inquire(err: inquire::InquireError) -> PromptError {
    match err {
        inquire::InquireError::OperationCanceled => PromptError::Cancelled,
        inquire::InquireError::OperationInterrupted => PromptError::Interrupted,
        other => PromptError::Failed(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::invocation::skot_args;

    /// Nobody: no terminal, no flags. The way `cargo test` always runs.
    fn silent() -> Asker {
        Asker::new(false, false, false)
    }

    #[test]
    fn a_question_with_a_default_takes_it_without_opening_a_prompt() {
        let opened = std::cell::Cell::new(false);
        let answer = silent()
            .ask("Default ref", Fallback::Default("main".to_owned()), || {
                opened.set(true);
                Ok("typed".to_owned())
            })
            .unwrap();
        assert_eq!(answer, "main");
        assert!(
            !opened.get(),
            "a run with nobody at the prompt must not open one"
        );
    }

    #[test]
    fn a_question_only_a_flag_can_answer_names_the_flag() {
        let err = silent()
            .text(
                "Which patches to enable",
                None,
                None,
                Fallback::Flag(skot_args("enable", "<patch>")),
                None,
            )
            .unwrap_err();
        let shown = err.to_string();
        assert!(
            shown.contains("cannot ask Which patches to enable"),
            "{shown}"
        );
        assert!(shown.contains(&skot_args("enable", "<patch>")), "{shown}");
    }

    #[test]
    fn yes_answers_the_yes_only_question_and_takes_the_default_of_the_rest() {
        // Even on a terminal: `--yes` means nobody is waiting.
        let yes = Asker::new(false, true, true);
        assert!(
            yes.confirm(
                "Drop a.patch and delete its file?",
                Some("Esc keeps it."),
                Fallback::Flag(skot_args("drop", "<patch> --yes")),
            )
            .unwrap(),
            "`--yes` answers the confirmation only `--yes` can answer"
        );
        assert!(
            !yes.confirm(
                "Configure hooks?",
                Some("All optional."),
                Fallback::Default(false),
            )
            .unwrap(),
            "a question with a default takes the default, `--yes` or not"
        );
    }

    #[test]
    fn yes_does_not_invent_an_answer_it_cannot_know() {
        let yes = Asker::new(false, true, false);
        let err = yes
            .text(
                "Which patches to enable",
                None,
                None,
                Fallback::Flag(skot_args("enable", "<patch>")),
                None,
            )
            .unwrap_err();
        assert!(
            err.to_string().contains(&skot_args("enable", "<patch>")),
            "`--yes` answers no question about which: {err}"
        );
    }

    #[test]
    fn json_is_a_run_nobody_can_ask_even_on_a_terminal() {
        let json = Asker::new(true, false, true);
        let answer = json
            .text(
                "Default ref",
                None,
                None,
                Fallback::Default("main".to_owned()),
                None,
            )
            .unwrap();
        assert_eq!(answer, "main");
    }

    #[test]
    fn an_interactive_run_opens_the_prompt_and_takes_its_answer() {
        let interactive = Asker::new(false, false, true);
        let answer = interactive
            .ask("Default ref", Fallback::Default("main".to_owned()), || {
                Ok("typed".to_owned())
            })
            .unwrap();
        assert_eq!(answer, "typed", "the person's answer, not the default");
    }

    #[test]
    fn escaping_a_prompt_is_cancelled_rather_than_a_malfunction() {
        let interactive = Asker::new(false, false, true);
        let err = interactive
            .ask("Default ref", Fallback::Default(String::new()), || {
                Err(inquire::InquireError::OperationCanceled)
            })
            .unwrap_err();
        assert!(matches!(err, PromptError::Cancelled), "{err:?}");
        assert_eq!(err.to_string(), "cancelled");
    }
}
