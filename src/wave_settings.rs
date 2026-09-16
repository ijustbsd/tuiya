use crossterm::event::{KeyCode, KeyEvent};
use ratatui::widgets::TableState;

use crate::api::models::{WaveChoices, WaveSettings};

pub enum Action {
    None,
    Cancel,
    Apply,
}

pub struct WaveSettingsDialog {
    choices: WaveChoices,
    pub selected: usize,
    pub table: TableState,
}

impl WaveSettingsDialog {
    pub fn new(current: WaveSettings, choices: WaveChoices) -> Self {
        let selected = choices
            .options
            .iter()
            .position(|option| option == &current)
            .unwrap_or(0);
        Self {
            choices,
            selected,
            table: TableState::default(),
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        let len = self.choices.options.len();
        match key.code {
            KeyCode::Esc => return Action::Cancel,
            KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') | KeyCode::Right => {
                self.selected = (self.selected + 1) % len;
            }
            KeyCode::Up | KeyCode::BackTab | KeyCode::Char('k') | KeyCode::Left => {
                self.selected = (self.selected + len - 1) % len;
            }
            KeyCode::Enter | KeyCode::Char(' ') => return Action::Apply,
            _ => {}
        }
        Action::None
    }

    pub fn selected_settings(&self) -> WaveSettings {
        self.choices.options[self.selected].clone()
    }

    pub fn options(&self) -> &[WaveSettings] {
        &self.choices.options
    }

    pub fn help(&self) -> &str {
        &self.choices.options[self.selected].description
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(name: &str, seed: &str) -> WaveSettings {
        WaveSettings {
            name: name.into(),
            description: format!("About {name}"),
            seeds: vec![seed.into()],
        }
    }

    #[test]
    fn selects_a_server_preset_and_applies_explicitly() {
        let default = WaveSettings::default();
        let russian = settings("Only in Russian", "local-language:russian");
        let choices = WaveChoices {
            options: vec![default.clone(), russian.clone()],
        };
        let mut dialog = WaveSettingsDialog::new(default, choices);
        dialog.on_key(KeyCode::Down.into());
        assert_eq!(dialog.selected_settings(), russian);
        assert!(matches!(
            dialog.on_key(KeyCode::Enter.into()),
            Action::Apply
        ));
        assert!(matches!(dialog.on_key(KeyCode::Esc.into()), Action::Cancel));
    }
}
