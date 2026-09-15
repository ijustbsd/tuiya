use anyhow::{Context, Result, bail};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::TableState;

use crate::config::Preferences;

pub const ROWS: usize = 5;

pub enum Action {
    None,
    Cancel,
    Save,
}

pub struct Settings {
    pub draft: Preferences,
    pub cache_input: String,
    pub selected: usize,
    pub error: Option<String>,
    pub table: TableState,
    editing_cache: bool,
}

impl Settings {
    pub fn new(preferences: Preferences) -> Self {
        Self {
            cache_input: preferences.cache_limit_mb.to_string(),
            draft: preferences,
            selected: 0,
            error: None,
            table: TableState::default(),
            editing_cache: false,
        }
    }

    pub fn values(&self) -> Result<Preferences> {
        let cache_limit_mb: u64 = self
            .cache_input
            .parse()
            .context("Enter a whole cache size in MB.")?;
        if cache_limit_mb == 0 || cache_limit_mb.checked_mul(1024 * 1024).is_none() {
            bail!("Cache size must be positive and fit in bytes.");
        }
        let mut values = self.draft.clone();
        values.cache_limit_mb = cache_limit_mb;
        Ok(values)
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            return Action::Save;
        }
        match key.code {
            KeyCode::Esc => return Action::Cancel,
            KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => {
                self.selected = (self.selected + 1) % ROWS;
                self.editing_cache = false;
            }
            KeyCode::Up | KeyCode::BackTab | KeyCode::Char('k') => {
                self.selected = (self.selected + ROWS - 1) % ROWS;
                self.editing_cache = false;
            }
            KeyCode::Enter if self.selected == ROWS - 1 => return Action::Save,
            KeyCode::Enter | KeyCode::Char(' ') if self.selected < 2 => self.adjust(1),
            KeyCode::Enter => self.selected = ROWS - 1,
            KeyCode::Left => self.adjust(-1),
            KeyCode::Right => self.adjust(1),
            KeyCode::Char(digit) if self.selected == 2 && digit.is_ascii_digit() => {
                if !self.editing_cache {
                    self.cache_input.clear();
                    self.editing_cache = true;
                }
                if self.cache_input.len() < 20 {
                    self.cache_input.push(digit);
                }
                self.error = None;
            }
            KeyCode::Backspace if self.selected == 2 => {
                self.cache_input.pop();
                self.editing_cache = true;
                self.error = None;
            }
            _ => {}
        }
        Action::None
    }

    fn adjust(&mut self, direction: i32) {
        match self.selected {
            0 => {
                self.draft.quality = if self.draft.quality == "high" {
                    "lossless"
                } else {
                    "high"
                }
                .into()
            }
            1 => self.draft.streaming = !self.draft.streaming,
            2 => {
                let current = self.cache_input.parse::<u64>().unwrap_or(0);
                let value = if direction > 0 {
                    current.saturating_add(256)
                } else {
                    current.saturating_sub(256).max(1)
                };
                self.cache_input = value.to_string();
                self.editing_cache = false;
            }
            3 => {
                let percent = (self.draft.volume * 100.0).round() as i32;
                self.draft.volume = (percent + direction * 5).clamp(0, 200) as f32 / 100.0;
            }
            _ => {}
        }
        self.error = None;
    }

    pub fn help(&self) -> &'static str {
        match self.selected {
            0 => "Lossless prefers FLAC; High uses MP3 320. Applies to the next track.",
            1 => {
                "On starts during download; Off waits for the whole file. Applies to the next track."
            }
            2 => {
                "Type a size in MB; ←/→ steps by 256 MB. The playing track and ongoing downloads are kept."
            }
            3 => "←/→ changes volume by 5%. Applies when saved; startup uses this volume too.",
            _ => "Save changes to your config and apply them to the player.",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dialog() -> Settings {
        Settings::new(Preferences {
            quality: "lossless".into(),
            cache_limit_mb: 4096,
            streaming: true,
            volume: 1.0,
        })
    }

    #[test]
    fn edits_are_a_draft_until_explicit_save_and_can_be_cancelled() {
        let mut settings = dialog();
        let original = settings.draft.clone();
        settings.on_key(KeyCode::Right.into());
        settings.on_key(KeyCode::Down.into());
        settings.on_key(KeyCode::Char(' ').into());
        settings.on_key(KeyCode::Tab.into());
        for digit in "512".chars() {
            settings.on_key(KeyCode::Char(digit).into());
        }
        let values = settings.values().unwrap();
        assert_eq!(values.quality, "high");
        assert!(!values.streaming);
        assert_eq!(values.cache_limit_mb, 512);
        assert_eq!(original.cache_limit_mb, 4096);
        assert!(matches!(
            settings.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            Action::Save
        ));
        assert!(matches!(
            settings.on_key(KeyCode::Esc.into()),
            Action::Cancel
        ));
    }

    #[test]
    fn cache_validation_and_volume_bounds_prevent_invalid_values() {
        let mut settings = dialog();
        for value in ["", "0", "abc", "18446744073709551615"] {
            settings.cache_input = value.into();
            assert!(settings.values().is_err());
        }
        settings.selected = 3;
        for _ in 0..100 {
            settings.on_key(KeyCode::Right.into());
        }
        assert_eq!(settings.draft.volume, 2.0);
        for _ in 0..100 {
            settings.on_key(KeyCode::Left.into());
        }
        assert_eq!(settings.draft.volume, 0.0);
        settings.selected = ROWS - 1;
        assert!(matches!(
            settings.on_key(KeyCode::Enter.into()),
            Action::Save
        ));
    }
}
