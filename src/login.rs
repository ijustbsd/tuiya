use std::io::{IsTerminal, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal;

use crate::api::Client;
use crate::config::Config;

const AUTHORIZE_URL: &str = "https://oauth.yandex.ru/authorize?response_type=token&client_id=23cabbbdc6cd418abb4b39c32c41195d";
const MAX_INPUT: usize = 16 * 1024;

pub async fn run(config: &mut Config) -> Result<Client> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("Login needs an interactive terminal. Run tuiya login, or set TUIYA_TOKEN.");
    }
    println!("Sign in to Yandex in your browser and allow access to Yandex Music.");
    println!("{AUTHORIZE_URL}\n");
    if !open_browser() {
        println!("Open the link above in your browser.");
    }
    println!("After the redirect, copy the full address from the browser's address bar.");
    println!("Paste it here and press Enter. Input is hidden; Ctrl-C cancels.\n");
    let token = loop {
        print!("Redirect URL (or token): ");
        std::io::stdout().flush()?;
        let input = read_hidden();
        println!();
        match parse_token(&input?) {
            Ok(token) => break token,
            Err(error) => println!("{error}\nTry pasting the full address after signing in.\n"),
        }
    };
    println!("Checking access to Yandex Music…");
    let client = tokio::time::timeout(
        Duration::from_secs(30),
        Client::new(&token, config.api_quality(), config.codecs()),
    )
    .await
    .context("Login check timed out; the token has not been saved")?
    .context("Login failed; the token has not been saved")?;
    let path = Config::save_token(&token)?;
    config.token = token;
    println!(
        "Signed in as {}. Saved login to {}.",
        client.display_name,
        path.display()
    );
    if std::env::var_os("TUIYA_TOKEN").is_some() {
        println!("TUIYA_TOKEN is set and will override this saved login on future runs.");
    }
    Ok(client)
}

fn open_browser() -> bool {
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(not(target_os = "macos"))]
    let program = "xdg-open";
    let Ok(mut browser) = Command::new(program)
        .arg(AUTHORIZE_URL)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    // Some browser launchers stay alive until the browser closes.
    std::thread::spawn(move || {
        let _ = browser.wait();
    });
    true
}

struct HiddenInput {
    restore_raw: bool,
}

impl Drop for HiddenInput {
    fn drop(&mut self) {
        let _ = crossterm::execute!(std::io::stdout(), event::DisableBracketedPaste);
        if self.restore_raw {
            let _ = terminal::disable_raw_mode();
        }
    }
}

fn read_hidden() -> Result<String> {
    let restore_raw = !terminal::is_raw_mode_enabled()?;
    terminal::enable_raw_mode().context("cannot hide login input")?;
    let _guard = HiddenInput { restore_raw };
    crossterm::execute!(std::io::stdout(), event::EnableBracketedPaste)?;
    let mut input = String::new();
    loop {
        match event::read().context("cannot read login input")? {
            Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                KeyCode::Enter => return Ok(input),
                KeyCode::Esc => bail!("Login cancelled"),
                KeyCode::Char('c' | 'd') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    bail!("Login cancelled");
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    input.clear()
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(ch)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    input.push(ch)
                }
                _ => {}
            },
            Event::Paste(text) => input.push_str(&text),
            _ => {}
        }
        if input.len() > MAX_INPUT {
            bail!("Login input is too long");
        }
    }
}

/// Never include input or URL parser errors in diagnostics: either may contain
/// the credential. query_pairs percent-decodes the OAuth fragment for us.
fn parse_token(input: &str) -> Result<String> {
    let input = input.trim();
    if input.is_empty() || input.chars().any(char::is_whitespace) {
        bail!("Paste a redirect URL or OAuth token without spaces.");
    }
    let token = if input.contains("://") {
        let url =
            reqwest::Url::parse(input).map_err(|_| anyhow::anyhow!("Invalid redirect URL."))?;
        if !matches!(url.scheme(), "https" | "http") {
            bail!("Expected a browser redirect URL.");
        }
        token_from_fragment(
            url.fragment()
                .context("The address has no OAuth token fragment.")?,
        )?
    } else if input.trim_start_matches('#').starts_with("access_token=") {
        token_from_fragment(input.trim_start_matches('#'))?
    } else {
        input.to_owned()
    };
    if token.is_empty()
        || !token
            .bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || b"-_.:".contains(&ch))
    {
        bail!("Invalid OAuth token. Copy the full address after the redirect.");
    }
    Ok(token)
}

fn token_from_fragment(fragment: &str) -> Result<String> {
    let mut parameters = reqwest::Url::parse("https://localhost/").expect("static URL is valid");
    parameters.set_query(Some(fragment));
    let mut token = None;
    for (key, value) in parameters.query_pairs() {
        if key == "error" {
            bail!("Yandex did not grant access. Try signing in again.");
        }
        if key == "access_token" {
            if token.is_some() {
                bail!("The address contains more than one access_token.");
            }
            token = Some(value.into_owned());
        }
    }
    token.context("The address has no access_token. Copy the address after the redirect.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_tokens_from_redirects_fragments_and_direct_input() {
        for input in [
            " https://music.yandex.ru/#access_token=y0__test-token&token_type=bearer&expires_in=123 \n",
            "https://music.yandex.ru/#expires_in=123&access_token=y0%5F%5Ftest-token",
            "#access_token=y0__test-token&token_type=bearer",
            "access_token=y0__test-token",
            "y0__test-token",
        ] {
            assert_eq!(parse_token(input).unwrap(), "y0__test-token");
        }
    }

    #[test]
    fn rejects_errors_duplicates_and_invalid_tokens_without_echoing_credentials() {
        for input in [
            "",
            "https://music.yandex.ru/",
            "https://music.yandex.ru/#access_token=",
            "https://music.yandex.ru/#access_token=secret-token&access_token=another",
            "https://music.yandex.ru/#error=access_denied&error_description=secret-token",
            "https://music.yandex.ru/#access_token=secret-token%0D%0A",
            "https://[invalid/#access_token=secret-token",
            "file://localhost/#access_token=secret-token",
            "secret-token with spaces",
        ] {
            let error = parse_token(input).unwrap_err();
            assert!(!format!("{error:#}").contains("secret-token"));
        }
    }
}
