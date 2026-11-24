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

## Monitoring

Lynx serves a public health report for uptime monitors at the root of the Activity domain, outside the panel's Discord-only check:

- `GET https://<activity-domain>/health`: the whole report. HTTP `200` when the status is `ok` or `degraded`, `503` when it is `down`.
- `GET https://<activity-domain>/health/bots/N`: one bot (`0` is the main bot, `1..` the voice bots in `NODE_TOKENS` order). HTTP `200` while that bot is connected, `503` when it is down, `404` for an unknown number.

```json
{
  "status": "ok",
  "version": "0.1.0",
  "commit": "abc1234",
  "uptimeSecs": 12345,
  "checkedAt": "2026-11-24T12:00:00Z",
  "bots": [
    { "index": 0, "role": "main", "name": "Lynx", "status": "ok", "connected": true,
      "shards": { "total": 1, "connected": 1 }, "gatewayPingMs": 42, "guilds": 120,
      "voiceSessions": 3, "downSince": null },
    { "index": 1, "role": "voice", "name": "Lynx 2", "status": "ok", "connected": true,
      "shards": { "total": 1, "connected": 1 }, "gatewayPingMs": 45, "guilds": 80,
      "voiceSessions": 2, "downSince": null }
  ],
  "services": {
    "database": { "ok": true, "latencyMs": 1 },
    "ytdlp": { "ok": true, "version": "2026.11.23" },
    "ollama": { "ok": true }
  },
  "players": { "active": 5, "playing": 3 }
}
```

`role` is `main` or `voice`; `connected` means every shard of that bot has a live gateway session; `downSince` is when the current outage started (RFC 3339, `null` while fully connected); `ollama` is `null` when Ollama is not configured; `ytdlp.version` is `null` for the first seconds after a start, before yt-dlp has been asked. The report contains no tokens, ids, server or channel names, user data or error details.

Status rules (a gateway outage shorter than 30 seconds is a normal reconnect and does not count yet):

| Status | When |
| --- | --- |
| `down` (503) | The main bot has had no gateway connection for over 30 s, or the database does not answer `SELECT 1` within 2 s. |
| `degraded` (200) | A voice bot has been disconnected for over 30 s, a shard of the main bot has been down for over 30 s, a bot's gateway ping is over 1000 ms, yt-dlp is missing or unusable, or Ollama is configured but does not answer. |
| `ok` (200) | Everything else. |

A bot's own `status` (used by `/health/bots/N`) follows the same rules for that bot alone. The report is computed at most once every 5 seconds and served from memory in between; the yt-dlp version comes from a cache refreshed every 15 minutes, and Ollama is checked with `/api/ps` (which never loads a model), reusing an answer up to a minute old. nginx limits these paths to 10 requests per second per IP (burst 20; over the limit answers `429`) and marks them `Cache-Control: no-store` and `X-Robots-Tag: noindex`. The internal `/backend/healthz` used by the Docker health check is unchanged.

### Better Uptime (Better Stack)

1. **Monitors → Create monitor**, type *URL becomes unavailable*, URL `https://<activity-domain>/health`. A `503` (down), a timeout or any other non-2xx answer alerts; `degraded` still answers `200`.
2. Optional, for partial outages: a second monitor on the same URL of type *URL doesn't contain keyword* / *Keyword should not exist* with the keyword `"degraded"` (with the quotes, so it matches the status value and never a bot name).
3. Optional, one monitor per bot: `https://<activity-domain>/health/bots/0` (main), `https://<activity-domain>/health/bots/1`, and so on, each of type *URL becomes unavailable*.
4. **Status pages → your page → Structure**: add a section "Discord bots" and put the monitors in it: "Lynx" (the `/health` monitor, plus the keyword monitor if you made it) and, optionally, one entry per bot. Lynx then shows green when everything works, degraded when the keyword monitor fires, and red when `/health` fails.

## Admin panel

An operator panel on its own port, separate from the Activity: status of every bot, an activity summary, the live log, notices / scheduled downtime and a few runtime switches. It is **off** unless `ADMIN_USERNAME` and a password are set. Its code lives in [admin/](admin/): the UI in `admin/web` (its own Vite app) and the backend in `admin/server` (compiled into the bot).

| Variable | Default | Meaning |
| --- | --- | --- |
| `ADMIN_USERNAME` | empty | Required to turn the admin panel on. |
| `ADMIN_PASSWORD` | empty | Plain password, at least 12 characters (shorter keeps the panel off, with a warning in the log). Leading and trailing spaces are ignored. |
| `ADMIN_PASSWORD_HASH` | empty | Argon2 hash of the password instead of the plain one (wins when both are set). Make it with `docker compose run --rm -T --entrypoint /app/lynx lynx hash-admin-password` (or `lynx hash-admin-password` outside Docker), type the password, and put the output in `.env` **inside single quotes** (it contains `$`). |
| `ADMIN_HTTP_HOST` / `ADMIN_HTTP_PORT` | `127.0.0.1` / `4001` | Internal admin listener. Only matters without Docker: the image always uses `127.0.0.1:4001`. |
| `ADMIN_COOKIE_SECURE` | `auto` | `auto`: the session cookie is `Secure` when the request came over HTTPS (`X-Forwarded-Proto: https` from your proxy). `true` / `false` force it. |
| `ADMIN_WEB_DIR` | empty | Folder of the built admin UI for the backend to serve itself (Windows dev run only; nginx serves it in the image). |

In Docker, nginx serves the admin UI on container port **8081** and proxies only that server's `/api/` to the admin listener; the Activity server on 8080 has no route to it. Publish it in `docker-compose.yml` (`- "81:8081"`, commented out by default). **Keep it off the internet**: publish it on a LAN address only (for example `"192.168.1.10:81:8081"`), or put it behind Nginx Proxy Manager with an access list and HTTPS.

Sections:

- **Overview**: status per bot (Main, Voice A, Voice B... with their names): connection, shards, ping, servers, voice sessions; totals right now (servers, players, players playing music, people listening); services (database, yt-dlp version and update state, Ollama); version, commit, uptime and memory. Refreshed every 10 seconds.
- **Activity**: the last hour (per minute) and the last 24 hours (per 15 minutes): slash commands by name, button clicks, panel requests by route group, tracks started and failed, searches, yt-dlp resolves with average and p95 resolve time and time to first audio, rate-limit refusals, warnings and errors. Kept in memory only (a restart starts from zero).
- **Logs**: the bot's log live (the last 2000 lines, then new ones as they come), with level colors, filters by level, source and text, pause, clear and download. Anything that looks like a token, an `Authorization` or `Cookie` value or a session token is redacted before it reaches the viewer.
- **Notices**: create, edit and delete notices of type scheduled downtime, information or warning, with a title, a message, a start time, a duration or end time, an optional "show from" time (default: right away) and an active switch. Times are entered in your local time and stored in UTC. The soonest visible notice is added as a small line to every message the bot sends in Discord (Discord timestamps, so each user reads it in their own time zone), and appears as a banner at the top of the Activity panel (live; each user can hide it). Notices disappear on their own after the end time and are deleted 30 days later.
- **Tweaks** (no restart, kept across restarts): maintenance mode (new songs are refused in `/play`, `/ai` and the panel, with the notice; what is playing keeps playing), pause the daily yt-dlp self-update, check for a yt-dlp update now, clear the in-memory stream URL cache, clear the lookup cache (saved searches, list pages, YouTube matches).

Security: both the username and the password are compared in constant time (or the password is checked against the Argon2 hash); sign-ins are limited to 5 per minute per address (and 30 per minute overall); the session is a random token in an `HttpOnly`, `SameSite=Strict` cookie (`Secure` over HTTPS) that expires after 12 hours (a restart signs you out); every change needs a custom header and a same-origin request (CSRF). The admin panel cannot read settings or secrets and cannot run commands.

Isolation: the admin listener runs on its own small runtime (two threads) on its own thread, with request timeouts, a body size limit, at most 32 requests in flight, 64 connections and 4 live log viewers. Handlers only read snapshots and counters the bot keeps anyway; database and yt-dlp work is handed to the bot with a short timeout. A panic in admin code answers 500; if the admin listener fails (for example its port is taken) it is logged and retried, and the bot keeps running either way. The log tee never waits for a viewer: a slow one skips lines.

## Development

The Rust toolchain runs inside Docker, so nothing needs to be installed on the host:

```sh
scripts/cargo.sh check
scripts/cargo.sh test
```

The panel lives in [web/](web/) (React, Vite, Tailwind). The admin UI is a separate app in [admin/web/](admin/web/) (`pnpm install`, `pnpm build`, `pnpm test`; `pnpm dev` runs it with live reload against a running admin listener on 127.0.0.1:8081).

On Windows without Docker, `scripts/dev-windows.ps1` runs the bot on `127.0.0.1:8080` and the panel on port 5173; with `ADMIN_USERNAME` and `ADMIN_PASSWORD` in `.env` the bot also serves the admin UI and API itself on `http://127.0.0.1:8081` (`-AdminPort` to change it).

## License

Lynx is licensed under the [GNU Affero General Public License v3.0](LICENSE) with the [Commons Clause](https://commonsclause.com/) condition. In short:

- You can use, study, modify and run Lynx for free, including hosting it for your own communities.
- If you modify Lynx and run it for others (even only as a hosted bot or website), you must publish your changes under the same license. It cannot be made proprietary.
- You may not sell it: no charging for Lynx itself, for hosting it, or for any product or service whose value comes from it.

The [LICENSE](LICENSE) file is the binding text; this summary is not legal advice.
