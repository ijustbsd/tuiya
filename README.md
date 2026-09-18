# tuiya

A fast, keyboard-driven terminal player for Yandex Music.

An unofficial third-party client, not affiliated with or endorsed by Yandex.

![tuiya logo](assets/tuiya-logo.png)

## Features

- [x] **Wave**
  - [x] My Wave with adaptive Rotor feedback, queue continuation, and playback
    history reporting.
  - [ ] Dislike tracks in My Wave.
- [x] **Library**
  - [x] Liked tracks with metadata, in order or shuffled.
  - [x] Like and unlike tracks from inside the player.
  - [ ] Playlists.
  - [ ] Liked playlists.
- [x] **Playback**
  - [x] Streaming playback with on-disk caching and next-track prefetch.
  - [x] Lossless quality with MP3 320 fallback.
  - [ ] Download tracks for offline listening or playback in another app
    (separate from the playback cache).
- [x] **Integrations**
  - [x] System media controls on Linux (MPRIS) and macOS (Now Playing),
    including play/pause, next/previous, seeking, and Linux volume control.
- [x] **Interface**
  - [x] Keyboard-driven TUI with event-based redraws and no autoplay on startup.
  - [ ] Beautiful design. We are working on it; the thuja is a good start.
- [ ] **Discovery and metadata**
  - [ ] Search.
  - [ ] Cover art and lyrics.

## Install

Install the latest release for Linux x86_64/aarch64 or macOS (Intel and Apple
Silicon). Requires `curl` and `tar`; Rust is not needed.

```sh
curl -fsSL https://raw.githubusercontent.com/ijustbsd/tuiya/master/install.sh | sh
```

The script installs `tuiya` into `~/.local/bin` without `sudo`. Add that directory
to your shell's `PATH` if needed:

```sh
export PATH="$HOME/.local/bin:$PATH"
tuiya
```

To update an installed player to the latest release:

```sh
tuiya self-update
```

This updates the running executable in its installation directory, checks the
downloaded binary before replacing it, and skips installation if your version
is already current or newer. It works without a Yandex Music token. The install
directory must be writable. For older releases without this command, run the
installer again.

Use `tuiya version` to print the installed version and `tuiya help` for usage.

From a checkout, you can also choose a release and destination:

```sh
./install.sh --version 0.1.0 --bin-dir "$HOME/.local/bin"
```

Linux binaries are built on Ubuntu 24.04 with glibc and require the ALSA runtime
library (`libasound2t64` on Ubuntu 24.04). A working audio output is required.

### Build from source

Requires Rust 1.93+. On Linux, install the ALSA development headers and
`pkg-config` first (`libasound2-dev` and `pkg-config` on Ubuntu).

```sh
cargo build --release --locked
./target/release/tuiya
```

## Configuration

On the first run, tuiya opens a browser sign-in. Allow access to Yandex Music,
then copy the full address after the redirect and paste it into the terminal.
Input is hidden. tuiya extracts and checks the token, then saves it with mode
`600`. You can also paste an OAuth token directly.

To sign in again or switch accounts:

```sh
tuiya login
```

If the browser does not open automatically, open the link printed in the
terminal. Press `Ctrl-C` to cancel. Login keeps your existing settings.

`~/.config/tuiya/config.toml`:

```toml
token = "y0_..."         # Yandex Music OAuth token
quality = "lossless"     # or "high" for MP3 320 only
cache_limit_mb = 4096    # cache limit for ~/.cache/tuiya
streaming = true         # false waits for the whole file before playing
volume = 1.0             # startup volume: 0.0–2.0 (100% = 1.0)
```

`TUIYA_TOKEN` overrides the token from the config file.

For manual sign-in, use [Yandex OAuth](https://oauth.yandex.ru/authorize?response_type=token&client_id=23cabbbdc6cd418abb4b39c32c41195d).
The token is the `access_token` value after `#` in the redirected URL.

The token grants full access to the account, so keep the config at mode `600`
and out of version control.
