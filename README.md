# Lynx

A Discord music bot with a built-in panel that opens as a Discord Activity.

- Play from YouTube, YouTube Music, Spotify and anything yt-dlp supports, streamed without downloading whole files.
- Manage the queue, seek, read synced lyrics and change settings from the Activity panel.
- Resumes where it left off after a restart, as long as someone is still in the voice channel.
- Can run several bots at once, so different voice channels in the same server each get their own music.
- Optional AI radio (`/ai`) based on your listening history, powered by your own Ollama server.

> Work in progress: this branch is a full rewrite of the previous PIXL/Rawon bot.

## Requirements

- Docker
- A Discord application for the main bot (and optionally more applications used as voice-only nodes)
- A public HTTPS URL for the Activity panel

## Running

```sh
cp .env.example .env   # fill in your tokens and URLs
docker compose up -d --build
```

All settings are environment variables; see [.env.example](.env.example).

### Saved playlists

Every playlist, album or artist page a user adds (with `/play` or from the panel) is saved to that user's Playlists tab in the panel: only its name, image, platform and URL. `MAX_STORED_PLAYLISTS` (default `20`, `0` turns saving off) caps the list per user; when it is full the least recently used unpinned entry is dropped, and when every entry is pinned new ones are not saved. Nothing is saved for users who turned their listening history off. The songs of a list are cached for 60 minutes per URL and shared by every user and server, and the panel's refresh button is limited per user (once every 5 seconds; spamming it blocks refreshes for 5 minutes), so popular lists do not flood Spotify or YouTube with requests.

### Ollama on a shared GPU

The AI radio and the genre tagger use Ollama only when a model is usable, so the GPU can be shared with other workloads:

| Variable | Default | Meaning |
| --- | --- | --- |
| `OLLAMA_URL` | empty | Ollama server. Empty: no LLM, the radio uses only the recommendation algorithm. |
| `OLLAMA_MODEL` | required with `OLLAMA_URL` | Preferred model. |
| `OLLAMA_CHECKED_MODELS` | empty | Comma-separated alternates that are also acceptable, in order of preference. |
| `OLLAMA_LOAD_RETRY_SECS` | `30` | Wait before retrying a failed load of the preferred model. |
| `OLLAMA_KEEP_ALIVE` | `30m` | How long Ollama keeps the preferred model loaded after use. |
| `OLLAMA_TIMEOUT_SECS` | `120` | Timeout of each model load and chat request. |

Before each use Lynx asks Ollama which models are loaded (`/api/ps`, cached for 30 seconds). If the preferred model is loaded it is used; otherwise, if any checked model is loaded, that one is used and nothing is loaded. Only when no acceptable model is in memory does Lynx load the preferred one. If that fails (for example, not enough GPU memory), it checks again for a checked model, waits `OLLAMA_LOAD_RETRY_SECS` (the radio shows "waiting in line"), checks once more and retries the load once. If that fails too, the radio continues with the algorithm alone. The tagger never waits: it uses a model only if one is already loaded or loads right away, and otherwise leaves the track for later. Lynx never unloads other models, and does not change the keep-alive of an alternate model it borrows.

On startup the main bot uploads its own button icons as application emojis of its Discord application (16 small PNGs from [assets/emojis/](assets/emojis/), only the missing ones; nothing is ever deleted). If that fails, the buttons simply show text only. To change the icons, edit and run `scripts/gen-emojis.mjs`; emojis are matched by name, so delete the old one in the Developer Portal (Emojis tab) for a redrawn icon to be uploaded again.

## Development

The Rust toolchain runs inside Docker, so nothing needs to be installed on the host:

```sh
scripts/cargo.sh check
scripts/cargo.sh test
```

The panel lives in [web/](web/) (React, Vite, Tailwind).

## License

Lynx is licensed under the [GNU Affero General Public License v3.0](LICENSE) with the [Commons Clause](https://commonsclause.com/) condition. In short:

- You can use, study, modify and run Lynx for free, including hosting it for your own communities.
- If you modify Lynx and run it for others (even only as a hosted bot or website), you must publish your changes under the same license. It cannot be made proprietary.
- You may not sell it: no charging for Lynx itself, for hosting it, or for any product or service whose value comes from it.

The [LICENSE](LICENSE) file is the binding text; this summary is not legal advice.
