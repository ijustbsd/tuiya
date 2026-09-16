use crossterm::event::{KeyCode, KeyEvent};
use ratatui::widgets::TableState;

use crate::api::models::{WaveOption, WaveRestrictions, WaveSettings};

pub const ROWS: usize = 4;

pub enum Action {
    None,
    Cancel,
    Apply,
}

pub struct WaveSettingsDialog {
    pub draft: WaveSettings,
    restrictions: WaveRestrictions,
    pub selected: usize,
    pub table: TableState,
}

impl WaveSettingsDialog {
    pub fn new(settings: WaveSettings, restrictions: WaveRestrictions) -> Self {
        Self {
            draft: settings,
            restrictions,
            selected: 0,
            table: TableState::default(),
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc => return Action::Cancel,
            KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => {
                self.selected = (self.selected + 1) % ROWS;
            }
            KeyCode::Up | KeyCode::BackTab | KeyCode::Char('k') => {
                self.selected = (self.selected + ROWS - 1) % ROWS;
            }
            KeyCode::Enter if self.selected == ROWS - 1 => return Action::Apply,
            KeyCode::Enter | KeyCode::Char(' ') if self.selected < ROWS - 1 => self.adjust(1),
            KeyCode::Left => self.adjust(-1),
            KeyCode::Right => self.adjust(1),
            _ => {}
        }
        Action::None
    }

    fn adjust(&mut self, direction: i32) {
        match self.selected {
            0 => cycle(
                &mut self.draft.language,
                &self.restrictions.language,
                direction,
            ),
            1 => cycle(
                &mut self.draft.mood_energy,
                &self.restrictions.mood_energy,
                direction,
            ),
            2 => cycle(
                &mut self.draft.diversity,
                &self.restrictions.diversity,
                direction,
            ),
            _ => {}
        }
    }

    pub fn help(&self) -> &'static str {
        match self.selected {
            0 => "Limit this Wave session by language, or choose instrumental music.",
            1 => "Tune this Wave session for a mood, or leave every mood in the mix.",
            2 => "Choose familiar favorites, discoveries, popular tracks, or a balanced mix.",
            _ => "Start a fresh Wave session with these choices. Nothing is saved to the account.",
        }
    }
}

fn cycle(value: &mut WaveOption, choices: &[WaveOption], direction: i32) {
    let current = choices
        .iter()
        .position(|choice| choice == value)
        .unwrap_or(0);
    let next = if direction >= 0 {
        (current + 1) % choices.len()
    } else {
        (current + choices.len() - 1) % choices.len()
    };
    *value = choices[next].clone();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_are_session_only_and_apply_explicitly() {
        let default = WaveOption {
            name: "Any".into(),
            seed: None,
        };
        let russian = WaveOption {
            name: "Russian".into(),
            seed: Some("settingLanguage:russian".into()),
        };
        let restrictions = WaveRestrictions {
            language: vec![default.clone(), russian.clone()],
            mood_energy: vec![default.clone()],
            diversity: vec![default.clone()],
        };
        let settings = restrictions.defaults().unwrap();
        let mut dialog = WaveSettingsDialog::new(settings, restrictions);
        dialog.on_key(KeyCode::Right.into());
        assert_eq!(dialog.draft.language, russian);
        dialog.selected = ROWS - 1;
        assert!(matches!(
            dialog.on_key(KeyCode::Enter.into()),
            Action::Apply
        ));
        assert!(matches!(dialog.on_key(KeyCode::Esc.into()), Action::Cancel));
    }
}
