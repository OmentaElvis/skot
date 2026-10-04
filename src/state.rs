//! Run state, so an interrupted run is reported rather than silently resumed.
//!
//! The pipeline this is modelled on handled an interrupted run by accident: the next
//! run force-reset the tree and happened to be fine. That works until it does not.
//! Here the phase is checkpointed, so `skot status` can say what was in flight.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::layout::Layout;
use crate::phase::Phase;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// The ref the sandbox is currently reset to.
    pub ref_name: Option<String>,
    /// The commit it was reset to.
    pub sha: Option<String>,
    /// Patches currently applied to the sandbox, in order.
    pub applied: Vec<String>,
    /// The phase a run reached. `Some` after an interrupted run.
    pub in_flight: Option<Phase>,
    /// The command that was running.
    pub in_flight_command: Option<String>,
}

impl State {
    pub fn load(layout: &Layout) -> Result<Self> {
        let path = layout.state_file();
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).context(format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).context(format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, layout: &Layout) -> Result<()> {
        layout.ensure_state_dirs()?;
        let path = layout.state_file();
        let text = serde_json::to_string_pretty(self)?;
        // Write-then-rename so an interrupted save cannot leave truncated state.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text).context(format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).context(format!("writing {}", path.display()))?;
        Ok(())
    }

    /// Record that a run has started.
    pub fn begin(&mut self, layout: &Layout, command: &str) -> Result<()> {
        self.in_flight_command = Some(command.to_owned());
        self.save(layout)
    }

    /// Record that a run reached `phase`.
    pub fn checkpoint(&mut self, layout: &Layout, phase: Phase) -> Result<()> {
        self.in_flight = Some(phase);
        self.save(layout)
    }

    /// Record that a run finished cleanly.
    pub fn finish(&mut self, layout: &Layout) -> Result<()> {
        self.in_flight = None;
        self.in_flight_command = None;
        self.save(layout)
    }

    /// Record the ref the sandbox is reset to.
    pub fn set_ref(&mut self, layout: &Layout, name: &str, sha: &str) -> Result<()> {
        self.ref_name = Some(name.to_owned());
        self.sha = Some(sha.to_owned());
        self.save(layout)
    }

    pub fn set_applied(&mut self, layout: &Layout, applied: Vec<String>) -> Result<()> {
        self.applied = applied;
        self.save(layout)
    }

    /// Forget the applied series.
    pub fn clear_applied(&mut self, layout: &Layout) -> Result<()> {
        self.applied.clear();
        self.save(layout)
    }

    /// A one-line description of an interrupted run, if there was one.
    pub fn interrupted(&self) -> Option<String> {
        let phase = self.in_flight?;
        let command = self.in_flight_command.as_deref().unwrap_or("skot");
        Some(match &self.ref_name {
            Some(r) => format!("`{command}` was interrupted during the {phase} phase (ref {r})"),
            None => format!("`{command}` was interrupted during the {phase} phase"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> (tempfile::TempDir, Layout) {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::new(tmp.path());
        (tmp, layout)
    }

    #[test]
    fn a_missing_state_file_is_an_empty_state() {
        let (_tmp, l) = layout();
        assert_eq!(State::load(&l).unwrap(), State::default());
    }

    #[test]
    fn state_round_trips_through_the_filesystem() {
        let (_tmp, l) = layout();
        let mut s = State::default();
        s.set_ref(&l, "v1.2.3", "abc123").unwrap();
        s.set_applied(&l, vec!["0001-a.patch".into()]).unwrap();

        assert_eq!(State::load(&l).unwrap(), s);
    }

    #[test]
    fn clearing_the_applied_series_forgets_it() {
        let (_tmp, l) = layout();
        let mut s = State::default();
        s.set_applied(&l, vec!["0001-a.patch".into()]).unwrap();
        s.set_ref(&l, "main", "abc123").unwrap();

        s.clear_applied(&l).unwrap();
        let reloaded = State::load(&l).unwrap();
        assert!(reloaded.applied.is_empty());
        // The ref survives: clearing the series does not unlearn what the project tracks.
        assert_eq!(reloaded.ref_name.as_deref(), Some("main"));
    }

    #[test]
    fn a_finished_run_leaves_nothing_in_flight() {
        let (_tmp, l) = layout();
        let mut s = State::default();
        s.begin(&l, "build").unwrap();
        s.checkpoint(&l, Phase::Build).unwrap();
        assert!(s.interrupted().is_some());

        s.finish(&l).unwrap();
        assert!(State::load(&l).unwrap().interrupted().is_none());
    }

    #[test]
    fn an_interrupted_run_names_the_command_the_phase_and_the_ref() {
        let (_tmp, l) = layout();
        let mut s = State::default();
        s.set_ref(&l, "v1.2.3", "abc").unwrap();
        s.begin(&l, "build").unwrap();
        s.checkpoint(&l, Phase::Deps).unwrap();

        let msg = s.interrupted().unwrap();
        assert!(msg.contains("build"), "got {msg}");
        assert!(msg.contains("deps"), "got {msg}");
        assert!(msg.contains("v1.2.3"), "got {msg}");
    }

    #[test]
    fn a_corrupt_state_file_is_an_error_not_a_silent_default() {
        let (_tmp, l) = layout();
        l.ensure_state_dirs().unwrap();
        std::fs::write(l.state_file(), "{ not json").unwrap();

        let err = State::load(&l).unwrap_err().to_string();
        assert!(err.contains("state.json"), "got {err}");
    }

    #[test]
    fn unknown_state_keys_do_not_break_loading() {
        let (_tmp, l) = layout();
        l.ensure_state_dirs().unwrap();
        std::fs::write(
            l.state_file(),
            r#"{"ref_name":"v1","from_a_newer_version":true}"#,
        )
        .unwrap();
        assert_eq!(State::load(&l).unwrap().ref_name.as_deref(), Some("v1"));
    }
}
