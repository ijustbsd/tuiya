# tuiya

A console player for Yandex Music.

```
 tuiya  Wave   Liked
┌ My Wave ─────────────────────────────────────────────────────────────┐
│▶ 1   Future                      Mask Off                     ♥ 03:25│
│  2   Tony Yayo & Eminem          Drama Setter                   05:03│
│  3   NØRTHERN STRINGS            Keep Me Alive                  04:48│
└──────────────────────────────────────────────────────────────────────┘
──────────────────────────────────────────────────────────────────────
▶ Future — Mask Off  ♥
███████░░░░░░░░░░░░░░░░░░░░░░░░░░░░  00:34 / 03:25   🔊 100%
space pause · n next · b prev · ←/→ ±5s · l like · Tab switch · q quit
```

## What it does

- **My Wave** — the endless personal station. The player refills the queue on
  its own and reports events back to the station (`radioStarted`,
  `trackStarted`, `trackFinished`, `skip`) so its picks keep adapting to what
  you actually listen to.
- **Liked tracks** — the whole liked list with metadata, in order or shuffled.
- Like and unlike from inside the player.
- System media controls on Linux (MPRIS) and macOS (Now Playing): the current
  track appears in the desktop's media panel, with play/pause, next/previous
  and seeking. Linux also exposes volume control.
  Media keys work even when the terminal is not focused.
- Streaming playback: a track starts within a second or two instead of after
  its whole file has arrived.
- On-disk cache: a streamed track is kept once it finishes, and the next track
  is fetched ahead of time, so switching is instant. Old files are evicted once
  the cache outgrows its limit.
- `lossless` quality (FLAC in MP4), falling back to MP3 320 for tracks that
  have no lossless version.

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
```

`TUIYA_TOKEN` overrides the token from the config file.

For manual sign-in, use [Yandex OAuth](https://oauth.yandex.ru/authorize?response_type=token&client_id=23cabbbdc6cd418abb4b39c32c41195d).
The token is the `access_token` value after `#` in the redirected URL.

The token grants full access to the account, so keep the config at mode `600`
and out of version control.

## Keys

| Key | Action |
|---|---|
| `Tab`, `1`, `2` | switch tab |
| `j` / `k`, `↑` / `↓` | move through the list |
| `PgUp` / `PgDn`, `g` / `G` | page, jump to start / end |
| `Enter` | play the highlighted track |
| `space` | pause / resume |
| `n` | next track (counts as a skip for the wave) |
| `b` | previous track |
| `←` / `→` | seek 5 seconds |
| `+` / `-` | volume |
| `l` | like the playing track (or the highlighted one if nothing plays) |
| `s` | shuffle liked tracks |
| `r` | reload the liked list |
| `q`, `Esc`, `Ctrl-C` | quit |

On Linux, system media controls are registered automatically on the session
D-Bus as `org.mpris.MediaPlayer2.tuiya.instance<PID>`. GNOME and KDE can display
the track and route media keys to tuiya. You can also use
`playerctl --player=tuiya play-pause` (or `next`, `previous`, `position 5+`).
On window managers without media key handling, bind the keys to these commands.
Playback still works when the session bus is unavailable.

On macOS, tuiya publishes the track, artist, duration and playback position
to Now Playing in Control Center. System media keys and headphone controls
can play/pause and move between tracks while the terminal is unfocused.
Now Playing also supports changing the playback position. The system chooses
which active player receives media commands.

## How it works

```
api/      calls to api.music.yandex.net: wave, likes, signed file links
stream.rs a partially downloaded track that can be read and seeked
cache.rs  opens tracks for playback, downloads into ~/.cache/tuiya, eviction
audio.rs  a dedicated OS thread running rodio: decoding and playback
media/    Linux MPRIS and macOS Now Playing metadata and media commands
app.rs    state, queues, keyboard, background tasks
ui.rs     ratatui rendering
```

A link to an audio file is only handed out for an HMAC-SHA256-signed request
(`get-file-info`), so a plain GET will not get you one; the signature is
computed in `api::Client::download_info`.

Everything that touches the network runs in its own tokio task and comes back
to the UI as a message, so loading a 400-track list never blocks rendering.
Playback lives on its own OS thread because stopping rodio can block, and in
the render loop that would be visible.

Streaming keeps the download in memory and hands the decoder a reader over it.
Two regions are filled at once: the body arrives sequentially from the start,
while a second range request fetches the last 256 KB. The tail matters because
symphonia probes MP4 from the end — it jumps past `mdat` looking for trailing
atoms — so without it every lossless track would have to download in full
before its first sample. MP3 never reads there and skips the extra request.
Playback begins once 256 KB has arrived, which keeps the decoder, running on
the audio callback thread, from ever waiting on a read.

## Releases

Versions are not written down anywhere — they are worked out from commit
messages. Pushing to `master` runs the lints and tests, decides the next
version, builds binaries for Linux x86_64, Linux aarch64 and a universal
macOS binary, and publishes a GitHub release with notes generated from the
commits. `tuiya version` reports what it was built from; between tags it
says so, as in `0.2.0-3-gaecf094`.

This only works if commit subjects follow
[conventional commits](https://www.conventionalcommits.org):

| Prefix | Effect |
|---|---|
| `fix:` | patch release |
| `feat:` | minor release |
| `feat!:` or a `BREAKING CHANGE:` footer | major release |
| `chore:`, `docs:`, `ci:`, `refactor:`, `test:`, `style:` | no release |

A push carrying only the last row lands on `master` without cutting a
release, which is what `cliff.toml` is for. Nothing is ever committed back to
the branch: the version lives in the tag, and `build.rs` stamps it into the
binary, so `Cargo.toml` never needs touching.

## Known limitations

- Seeking forward is held to the part that has downloaded, and says so in the
  status line. Reading past it would block the audio thread and stall playback
  outright, which is worse than a short seek.
- Holding down the seek key drops most of the presses: rodio performs one seek
  at a time and a new request replaces the pending one.
- No dislike command for the wave.
- No cover art, lyrics or search.
