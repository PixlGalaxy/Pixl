# Lynx

A Discord music bot with a built-in panel that opens as a Discord Activity.

- Play from YouTube, YouTube Music, Spotify and anything yt-dlp supports, streamed without downloading whole files.
- Manage the queue, seek, read synced lyrics and change settings from the Activity panel.
- Resumes where it left off after a restart, as long as someone is still in the voice channel.
- Can run several bots at once, so different voice channels in the same server each get their own music.
- Optional AI radio (`/ai`) based on your listening history, powered by your own Ollama server.

> Work in progress: this branch is a full rewrite of the previous PIXL bot.

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
