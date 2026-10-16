//! Run state, so an interrupted run is reported rather than silently resumed.
//!
//! The pipeline this is modelled on handled an interrupted run by accident: the next
//! run force-reset the tree and happened to be fine. That works until it does not.
//! Here the phase is checkpointed, so `skot status` can say what was in flight.
//!
//! State splits into two halves. The run bookkeeping (what was in flight) and the
//! pointer to the active series are properties of the project. The ref, sha and applied
//! set are properties of a *sandbox*, and with `sandbox.per-series` a project has
//! several, so they live in a map keyed by the series that owns each sandbox. In shared
//! mode the map holds at most one record: the holder, whoever that is, which may not be
//! the active series if a switch chose `on-switch = "keep"`.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::layout::Layout;
use crate::phase::Phase;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// The series commands operate on when `--series` does not say otherwise.
    pub active_series: Option<String>,
    /// One record per sandbox that exists, keyed by the series that owns it.
    ///
    /// Shared mode keeps at most one entry (see the module docs); per-series mode keeps
    /// one per series that has a sandbox. A record equal to the default is removed
    /// rather than stored, so an empty entry never claims a sandbox that has never
    /// been applied.
    pub sandboxes: BTreeMap<String, SandboxState>,
    /// The sandbox scope the records were written under. A config change that flips
    /// `sandbox.per-series` describes different directories, so records from the other
    /// scope are dropped on load rather than misread.
    pub per_series_sandbox: bool,
    /// The phase a run reached. `Some` after an interrupted run.
    pub in_flight: Option<Phase>,
    /// The command that was running.
    pub in_flight_command: Option<String>,
}

/// What one sandbox holds: where it is reset to, and which patches are in it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxState {
    /// The ref the sandbox is currently reset to.
    pub ref_name: Option<String>,
    /// The commit it was reset to.
    pub sha: Option<String>,
    /// Patches currently applied to the sandbox, in order.
    pub applied: Vec<String>,
}

impl SandboxState {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

impl State {
    pub fn load(layout: &Layout) -> Result<Self> {
        let path = layout.state_file();
        let mut state = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).context(format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).context(format!("reading {}", path.display())),
        }?;
        if state.per_series_sandbox != layout.per_series_sandbox() {
            // The records name directories under the old scope. Keep the run
            // bookkeeping and the pointer; the sandboxes are rebuilt by the next apply.
            state.sandboxes.clear();
            state.per_series_sandbox = layout.per_series_sandbox();
        }
        Ok(state)
    }

    pub fn save(&self, layout: &Layout) -> Result<()> {
        layout.ensure_state_dirs()?;
        let path = layout.state_file();
        let mut to_write = self.clone();
        to_write.per_series_sandbox = layout.per_series_sandbox();
        let text = serde_json::to_string_pretty(&to_write)?;
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

    /// The series the pointer names, if one is recorded.
    pub fn active_series(&self) -> Option<&str> {
        self.active_series.as_deref()
    }

    /// Point commands at `series`. Written by `init`, `series create` and `series use`.
    pub fn set_active_series(&mut self, layout: &Layout, series: &str) -> Result<()> {
        self.active_series = Some(series.to_owned());
        self.save(layout)
    }

    /// The record describing the sandbox this layout uses, if there is one.
    ///
    /// Shared mode reads the single record, whoever holds it: after a
    /// `on-switch = "keep"` switch away from series A, that record is A's, and it is A's
    /// tree still sitting in the sandbox. Per-series mode reads this layout's own.
    pub fn sandbox(&self, layout: &Layout) -> Option<&SandboxState> {
        if layout.per_series_sandbox() {
            self.sandboxes.get(layout.series())
        } else {
            self.sandboxes.iter().next().map(|(_, state)| state)
        }
    }

    /// The series whose sandbox this layout is looking at, if any.
    ///
    /// In shared mode this may differ from the active series; that difference is what
    /// `push` and `pop` refuse on.
    pub fn sandbox_holder<'a>(&'a self, layout: &'a Layout) -> Option<&'a str> {
        if layout.per_series_sandbox() {
            self.sandboxes
                .contains_key(layout.series())
                .then_some(layout.series())
        } else {
            self.sandboxes.keys().next().map(String::as_str)
        }
    }

    /// The ref the sandbox is on, as this layout sees it.
    pub fn ref_name(&self, layout: &Layout) -> Option<&str> {
        self.sandbox(layout)?.ref_name.as_deref()
    }

    /// The commit the sandbox is at, as this layout sees it.
    pub fn sha(&self, layout: &Layout) -> Option<&str> {
        self.sandbox(layout)?.sha.as_deref()
    }

    /// The patches applied to the sandbox this layout uses. Empty when there is none.
    pub fn applied(&self, layout: &Layout) -> &[String] {
        self.sandbox(layout)
            .map(|s| s.applied.as_slice())
            .unwrap_or(&[])
    }

    /// Record the ref the sandbox is reset to.
    ///
    /// A write replaces the record for this layout's series. In shared mode that also
    /// means evicting any other series' record: there is one shared sandbox, and after
    /// this apply it holds *this* series, so leaving the previous holder's record in
    /// place would describe a tree that no longer exists.
    pub fn set_ref(&mut self, layout: &Layout, name: &str, sha: &str) -> Result<()> {
        let record = self.record_for(layout);
        record.ref_name = Some(name.to_owned());
        record.sha = Some(sha.to_owned());
        self.save(layout)
    }

    pub fn set_applied(&mut self, layout: &Layout, applied: Vec<String>) -> Result<()> {
        let record = self.record_for(layout);
        record.applied = applied;
        self.save(layout)
    }

    /// Forget the applied series of the sandbox this layout uses.
    pub fn clear_applied(&mut self, layout: &Layout) -> Result<()> {
        let key = if layout.per_series_sandbox() {
            Some(layout.series().to_owned())
        } else {
            self.sandboxes.keys().next().cloned()
        };
        if let Some(key) = key
            && let Some(record) = self.sandboxes.get_mut(&key)
        {
            record.applied.clear();
            if record.is_empty() {
                self.sandboxes.remove(&key);
            }
        }
        self.save(layout)
    }

    /// Forget every sandbox record: `clean sandbox` removes the directories themselves,
    /// so no record may go on claiming a ref or an applied set.
    pub fn clear_sandboxes(&mut self, layout: &Layout) -> Result<()> {
        self.sandboxes.clear();
        self.save(layout)
    }

    /// Follow patches through a renumbering: the applied set keeps pointing at the same
    /// files after [`crate::series::Series::insert_renumbering`] renames them.
    ///
    /// Only this layout's own record is touched. A renumbering renames files in the
    /// active series, so a shared sandbox held by a *different* series has nothing in
    /// it that the rename refers to.
    pub fn follow_renames(&mut self, layout: &Layout, renames: &[(String, String)]) -> Result<()> {
        let mut moved = false;
        if let Some(record) = self.sandboxes.get_mut(layout.series()) {
            for (old, new) in renames {
                for name in &mut record.applied {
                    if name == old {
                        *name = new.clone();
                        moved = true;
                    }
                }
            }
        }
        if moved {
            self.save(layout)?;
        }
        Ok(())
    }

    /// A one-line description of an interrupted run, if there was one.
    pub fn interrupted(&self) -> Option<String> {
        let phase = self.in_flight?;
        let command = self.in_flight_command.as_deref().unwrap_or("skot");
        Some(match &self.active_series {
            Some(s) => format!("`{command}` was interrupted during the {phase} phase (series {s})"),
            None => format!("`{command}` was interrupted during the {phase} phase"),
        })
    }

    /// The record writes go to, creating it and evicting a shared-mode predecessor.
    fn record_for(&mut self, layout: &Layout) -> &mut SandboxState {
        if !layout.per_series_sandbox() {
            // One shared sandbox: whoever held it before does not hold it now.
            let active = layout.series().to_owned();
            self.sandboxes.retain(|key, _| *key == active);
        }
        self.sandboxes
            .entry(layout.series().to_owned())
            .or_default()
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
        assert!(reloaded.applied(&l).is_empty());
        // The ref survives: clearing the series does not unlearn what the project tracks.
        assert_eq!(reloaded.ref_name(&l), Some("main"));
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
    fn an_interrupted_run_names_the_command_and_the_phase() {
        let (_tmp, l) = layout();
        let mut s = State::default();
        s.begin(&l, "build").unwrap();
        s.checkpoint(&l, Phase::Apply).unwrap();

        let msg = s.interrupted().unwrap();
        assert!(msg.contains("build"), "got {msg}");
        assert!(msg.contains("apply"), "got {msg}");
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
            r#"{"active_series":"main","from_a_newer_version":true}"#,
        )
        .unwrap();
        assert_eq!(State::load(&l).unwrap().active_series(), Some("main"));
    }

    #[test]
    fn the_active_series_pointer_round_trips() {
        let (_tmp, l) = layout();
        let mut s = State::default();
        assert_eq!(s.active_series(), None);
        s.set_active_series(&l, "experiment").unwrap();
        assert_eq!(State::load(&l).unwrap().active_series(), Some("experiment"));
    }

    #[test]
    fn a_shared_sandbox_record_names_its_holder_and_is_replaced_on_apply() {
        // Keep-switch semantics: series A holds the shared sandbox, then B applies
        // over it. There is one sandbox, so there is one record, and it must now
        // describe B rather than a tree that is gone.
        let (_tmp, l) = layout();
        let mut s = State::default();
        s.set_ref(&l, "main", "aaa").unwrap();
        s.set_applied(&l, vec!["0001-a.patch".into()]).unwrap();
        assert_eq!(s.sandbox_holder(&l), Some("default"));

        let b = l.with_series("b").unwrap();
        s.set_ref(&b, "v2", "bbb").unwrap();
        s.set_applied(&b, vec!["0001-x.patch".into()]).unwrap();

        assert_eq!(s.sandboxes.len(), 1, "shared mode holds one record");
        assert_eq!(s.sandbox_holder(&b), Some("b"));
        assert_eq!(s.applied(&b), ["0001-x.patch"]);
        // The old series no longer sees a sandbox through its own layout: the record
        // belongs to the holder, which the shared view still reports.
        assert_eq!(s.sandbox_holder(&l), Some("b"));
    }

    #[test]
    fn per_series_sandbox_records_live_side_by_side() {
        let (_tmp, l) = layout();
        let a = l.with_per_series_sandbox(true);
        let b = a.with_series("b").unwrap();

        let mut s = State::load(&a).unwrap();
        s.set_ref(&a, "main", "aaa").unwrap();
        s.set_applied(&a, vec!["0001-a.patch".into()]).unwrap();
        s.set_ref(&b, "v2", "bbb").unwrap();
        s.set_applied(&b, vec!["0001-b.patch".into()]).unwrap();

        assert_eq!(s.sandboxes.len(), 2);
        assert_eq!(s.applied(&a), ["0001-a.patch"]);
        assert_eq!(s.applied(&b), ["0001-b.patch"]);
        assert_eq!(s.sandbox_holder(&a), Some("default"));
        assert_eq!(s.sandbox_holder(&b), Some("b"));
        // A series with no sandbox sees none, even though others have one.
        let c = a.with_series("c").unwrap();
        assert_eq!(s.sandbox(&c), None);
        assert_eq!(s.sandbox_holder(&c), None);
    }

    #[test]
    fn changing_the_sandbox_scope_drops_records_from_the_old_scope() {
        // The records name directories the new scope does not have: shared records live
        // at `.skot/sandbox`, per-series at `.skot/sandbox/<name>`. Misreading one for
        // the other would report a ref the tree never had.
        let (_tmp, l) = layout();
        let mut s = State::default();
        s.set_ref(&l, "main", "aaa").unwrap();
        s.set_applied(&l, vec!["0001-a.patch".into()]).unwrap();
        s.set_active_series(&l, "default").unwrap();

        let per = l.with_per_series_sandbox(true);
        let reloaded = State::load(&per).unwrap();
        assert!(reloaded.sandboxes.is_empty(), "old-scope records dropped");
        assert_eq!(
            reloaded.active_series(),
            Some("default"),
            "the pointer is a project fact, not a sandbox fact"
        );
        assert_eq!(reloaded.in_flight, s.in_flight);
    }

    #[test]
    fn renames_follow_the_active_series_record_only() {
        let (_tmp, l) = layout();
        let mut s = State::default();
        s.set_applied(&l, vec!["0001-a.patch".into(), "0002-b.patch".into()])
            .unwrap();

        s.follow_renames(
            &l,
            &[("0001-a.patch".to_owned(), "0001-renamed.patch".to_owned())],
        )
        .unwrap();

        assert_eq!(
            s.applied(&l),
            ["0001-renamed.patch", "0002-b.patch"],
            "the record must follow the files it names"
        );
    }
}
