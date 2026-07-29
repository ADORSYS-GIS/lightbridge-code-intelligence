//! The Repo Settings "page" sub-model (story #500): the selected repo's currently-configured review
//! preset, plus a free-text input to change it. Opened from a Repositories row (`s`), mirrors
//! `DetailState`'s "own life cycle" shape — self-contained, its own fetch + save flow.

use crate::api::PresetInfo;

pub struct RepoSettingsState {
    /// The repo this page is about.
    pub repo_id: i64,
    /// `owner/name`, for the panel title (seeded from the Repositories row so it renders immediately,
    /// before the preset fetch resolves).
    pub repo_label: String,
    /// The currently-configured preset, once fetched. `None` before the fetch resolves OR when the
    /// repo declares none (platform defaults apply) — [`Self::loaded`] distinguishes the two.
    pub current: Option<PresetInfo>,
    /// True once the initial fetch has resolved (loading vs. "confirmed nothing configured").
    pub loaded: bool,
    /// The free-text input buffer for the new preset name.
    pub input: String,
    /// True while a save (`POST .../preset`) is in flight — blocks a second submit.
    pub saving: bool,
}

impl RepoSettingsState {
    pub fn new(repo_id: i64, repo_label: String) -> Self {
        Self {
            repo_id,
            repo_label,
            current: None,
            loaded: false,
            input: String::new(),
            saving: false,
        }
    }

    /// Fold a resolved preset fetch into state. Pre-fills the input with the current preset (if any)
    /// so Enter-with-no-edits is a harmless no-op-shaped resubmit, not an accidental blank.
    pub fn set_loaded(&mut self, info: PresetInfo) {
        if let Some(preset) = &info.preset {
            self.input = preset.clone();
        }
        self.current = Some(info);
        self.loaded = true;
    }

    pub fn push_char(&mut self, c: char) {
        // Preset names are short bare identifiers (`fast`/`deep`/`ultra`/an operator-defined name) —
        // reject whitespace/control chars at the input boundary rather than allowing an obviously
        // invalid value the server would reject anyway.
        if !c.is_whitespace() && !c.is_control() {
            self.input.push(c);
        }
    }

    pub fn backspace(&mut self) {
        self.input.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_loaded_prefills_input_from_the_current_preset() {
        let mut s = RepoSettingsState::new(1, "o/r".into());
        assert!(!s.loaded);
        s.set_loaded(PresetInfo {
            preset: Some("ultra".to_string()),
            entry_points: Default::default(),
        });
        assert!(s.loaded);
        assert_eq!(s.input, "ultra");
    }

    #[test]
    fn set_loaded_with_no_configured_preset_leaves_input_empty() {
        let mut s = RepoSettingsState::new(1, "o/r".into());
        s.set_loaded(PresetInfo::default());
        assert!(s.loaded);
        assert_eq!(s.input, "");
        assert!(s.current.as_ref().unwrap().preset.is_none());
    }

    #[test]
    fn push_char_rejects_whitespace_and_control_chars() {
        let mut s = RepoSettingsState::new(1, "o/r".into());
        s.push_char('u');
        s.push_char(' ');
        s.push_char('l');
        s.push_char('\t');
        s.push_char('t');
        assert_eq!(s.input, "ult");
    }

    #[test]
    fn backspace_pops_the_last_char() {
        let mut s = RepoSettingsState::new(1, "o/r".into());
        s.push_char('a');
        s.push_char('b');
        s.backspace();
        assert_eq!(s.input, "a");
        s.backspace();
        s.backspace(); // no-op on empty
        assert_eq!(s.input, "");
    }
}
