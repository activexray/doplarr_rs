<p align="center">
  <img src="logos/logo-with-text.svg" alt="Doplarr" width="480">
</p>

<p align="center">
  <a href="https://github.com/activexray/doplarr_rs/actions/workflows/ci.yml"><img alt="CI" src="https://img.shields.io/github/actions/workflow/status/activexray/doplarr_rs/ci.yml?style=for-the-badge"></a>
  <a href="https://discord.gg/890634173751119882"><img alt="Discord" src="https://img.shields.io/discord/890634173751119882?color=ff69b4&label=discord&style=for-the-badge"></a>
  <a href="LICENSE-MIT"><img alt="License" src="https://img.shields.io/badge/license-MIT%2FApache--2.0-blue?style=for-the-badge"></a>
</p>

A Discord bot for requesting media through \*arr backends, written in Rust.

Each backend you configure creates a `/request <media>` slash command — e.g. `/request movie` and `/request series`.

## Screenshots

<p align="center">
  <img src="screenshots/series.png" alt="Series selection interface" width="400">
</p>

## Setup

### 1. Create a Discord bot

Go to the [Discord Developer Portal](https://discord.com/developers/applications), create a new application, then open the **Bot** tab and create a bot. Copy the token — you'll need it in your config.

Under **OAuth2 → URL Generator**, tick the `bot` and `applications.commands` scopes. Open the generated URL to invite the bot to your server.

> [!NOTE]
> To post request confirmations in the channel for everyone to see, also tick the `Send Messages` permission. Without it, requests still work — the public announcement is just skipped.

### 2. Get your backend API keys

- **Sonarr / Radarr**: Settings → General → Security → API Key
- **Seerr**: Settings → API Key — must be an **admin** key

### 3. Configure and run

Create a `config.toml` (see [Configuration](#configuration) below), then start the bot with Docker Compose:

```yaml
services:
  doplarr:
    image: ghcr.io/activexray/doplarr_rs:latest
    container_name: doplarr
    restart: unless-stopped
    volumes:
      - ./config.toml:/config.toml:ro
```

Commands register automatically on startup. If they don't appear right away, wait a minute or restart your Discord client.

## Configuration

Doplarr reads a single `config.toml`. A minimal one looks like this:

```toml
discord_token = "YOUR_DISCORD_BOT_TOKEN"

# Movies via Radarr → /request movie
[[backends]]
media = "movie"

[backends.config.Radarr]
url = "http://localhost:7878"
api_key = "your_radarr_api_key"

# TV via Sonarr → /request series
[[backends]]
media = "series"

[backends.config.Sonarr]
url = "http://localhost:8989"
api_key = "your_sonarr_api_key"
```

Each `[[backends]]` block adds one `/request <media>` command. Any option you
leave out (quality profile, root folder, …) is simply asked for in Discord at
request time.

That's all most setups need. For the **full list of options** — plus Seerr, 4K,
anime, and pointing several commands at one instance — see the annotated
**[config.example.toml](config.example.toml)**.

> [!TIP]
> - **No config yet?** Start the bot without one and it writes a starter
>   `config.toml` for you to edit.
> - **Keep secrets out of the file** by referencing environment variables:
>   `api_key = "${RADARR_API_KEY}"`.
> - **Coming from the Clojure Doplarr?** Your old environment variables still
>   work with no config file at all — see **[MIGRATING.md](MIGRATING.md)**.

### Using Seerr

> [!IMPORTANT]
> If you request through Seerr/Overseerr/Jellyseerr, each user must link their
> Discord account, or their requests are rejected. In Seerr, enable the Discord
> notification agent (Settings → Notifications → Discord), then each user enters
> their Discord User ID on their profile. To accept requests from unlinked users
> instead, set `fallback_user_id` in the config.

## Running as a Service

```ini
# /etc/systemd/system/doplarr.service
[Unit]
Description=Doplarr Discord Bot
After=network.target

[Service]
Type=simple
User=doplarr
Group=doplarr
ExecStart=/opt/doplarr/doplarr /opt/doplarr/config.toml
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now doplarr
```

## Building from Source

**With Nix:**

```bash
nix build
nix run . /path/to/config.toml
```

**With Cargo** (requires Rust, OpenSSL dev libraries, and pkg-config on Linux):

```bash
cargo build --release
./target/release/doplarr /path/to/config.toml
```

## Troubleshooting

**Bot doesn't respond to commands**
- Make sure you invited the bot with both `bot` and `applications.commands` scopes
- Commands register on startup — wait a minute or restart Discord if they're missing
- Check logs for connection errors

**Backend connection errors**
- Test your API keys directly in the \*arr web UI
- If running in Docker, make sure the container can reach your \*arr services (check network/hostname)
- Quality profile names are case-sensitive and must match exactly what's in Sonarr/Radarr settings

**Seerr: "user not found" or requests rejected**
1. Enable the Discord notification agent in Seerr (Settings → Notifications → Discord)
2. Each user goes to their Seerr profile → Settings → Notifications → Discord and enters their Discord User ID
3. Or set `fallback_user_id` in your config to accept requests from unlinked users

**Config parse errors**
- Validate your TOML syntax (e.g. [jsonformatter.org/toml-validator](https://jsonformatter.org/toml-validator))
- `discord_token` and at least one `[[backends]]` entry are required
- Every `media` command name must be unique, even across a backend's own aliases

## Migrating from the Clojure version

See [MIGRATING.md](MIGRATING.md) for the full config mapping from the old EDN format to TOML, renamed options, what's been removed, and running from environment variables.

## Development

See [README_DEVELOPER.md](README_DEVELOPER.md) for adding new backends, generating API bindings, and contributing.

## License

Licensed under either of Apache License 2.0 ([LICENSE-APACHE](LICENSE-APACHE)) or MIT License ([LICENSE-MIT](LICENSE-MIT)) at your option.

## Acknowledgments

- [Twilight](https://github.com/twilight-rs/twilight) for Discord API bindings
- [OpenAPI Generator](https://github.com/OpenAPITools/openapi-generator) for \*arr API clients
