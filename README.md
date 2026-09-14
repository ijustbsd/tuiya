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
- Streaming playback: a track starts within a second or two instead of after
  its whole file has arrived.
- On-disk cache: a streamed track is kept once it finishes, and the next track
  is fetched ahead of time, so switching is instant. Old files are evicted once
  the cache outgrows its limit.
- `lossless` quality (FLAC in MP4), falling back to MP3 320 for tracks that
  have no lossless version.

## Install

Requires Rust 1.93+ and a working audio output (ALSA/PipeWire/PulseAudio).

```sh
cargo build --release
./target/release/tuiya
```

## Configuration

`~/.config/tuiya/config.toml`:

```toml
token = "y0_..."         # Yandex Music OAuth token
quality = "lossless"     # or "high" for MP3 320 only
cache_limit_mb = 4096    # cache limit for ~/.cache/tuiya
streaming = true         # false waits for the whole file before playing
```

`TUIYA_TOKEN` overrides the token from the config file.

To get a token: open music.yandex.ru, look at any request to
`api.music.yandex.net` in the developer tools and copy the value out of the
`Authorization: OAuth <token>` header.

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

## How it works

```
api/      calls to api.music.yandex.net: wave, likes, signed file links
stream.rs a partially downloaded track that can be read and seeked
cache.rs  opens tracks for playback, downloads into ~/.cache/tuiya, eviction
audio.rs  a dedicated OS thread running rodio: decoding and playback
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

## Known limitations

- Seeking forward is held to the part that has downloaded, and says so in the
  status line. Reading past it would block the audio thread and stall playback
  outright, which is worse than a short seek.
- Holding down the seek key drops most of the presses: rodio performs one seek
  at a time and a new request replaces the pending one.
- No dislike command for the wave.
- No cover art, lyrics or search.
