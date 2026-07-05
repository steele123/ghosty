# Ghosty

Ghosty is a Riot Client presence spoofing tool that allows you to:
launch the Riot Client through a patched client-config URL, proxy Riot chat
traffic locally, and rewrite global presence so you can appear online, offline,
or mobile while keeping lobby and select chat available.

Inspired by [molenzwiebel/Deceive](https://github.com/molenzwiebel/Deceive).

## Features

- Tauri desktop control panel built with Svelte 5.
- Launch targets for League of Legends, VALORANT, Legends of Runeterra, 2XKO,
  or the Riot Client by itself.
- Local client-config proxy that rewrites Riot chat host/port settings.
- Local TLS chat proxy that filters XMPP presence stanzas.
- Presence modes: online, offline, and mobile.
- Startup status preference and session status persistence.
- Riot Client path discovery through `RiotClientInstalls.json`.
- Optional process killer for Riot Client/game processes.

## Development

```powershell
bun install
bun run check
bun run build
cd src-tauri
cargo check
```

Run the desktop app with:

```powershell
bun run tauri dev
```

The proxy depends on the same localhost certificate endpoint used by Deceive and
expects `deceive-localhost.molenzwiebel.xyz` to resolve to `127.0.0.1`.

## Command Reference

### App Controls

| Command | What it does |
| --- | --- |
| `Start` | Starts Ghosty's local config/chat proxy and launches the selected Riot target. |
| `Stop` | Stops the active Ghosty proxy session. |
| `Locate` | Finds the Riot Client path from local Riot install metadata. |
| `Kill Riot` | Terminates running Riot Client/game processes. |
| `Clean Restart` | Stops Riot processes, restarts Ghosty, and launches the selected target. |
| `Run Checks` | Runs preflight checks for local setup and proxy readiness. |
| `Call` | Sends the selected request to the League Client API from the Debug tab. |
| `Copy Logs` | Copies the current app log buffer to the clipboard. |
| `Copy Stream` | Copies the current XMPP event stream buffer to the clipboard. |

### Helper Friend Chat Commands

Send these as messages to the Ghosty helper friend in Riot chat.

| Command | What it does |
| --- | --- |
| `online` | Sets your masked Riot presence to online. |
| `offline` | Sets your masked Riot presence to offline. |
| `mobile` | Sets your masked Riot presence to mobile. |
| `enable` | Enables outgoing presence masking. |
| `disable` | Disables outgoing presence masking. |
| `status` | Replies with current presence masking, selected presence, auto accept, and client state. |
| `help` | Replies with the supported helper commands. |
| `friends` | Replies with a friends-list summary grouped by status/product. |
| `auto accept on` | Enables auto accept. Phrases containing `auto` and `accept` also turn it on by default. |
| `auto accept off` | Disables auto accept. Also accepts phrases with `auto accept off` or `auto accept disable`. |
| `auto accept status` | Replies with the current auto accept state. |
| `opgg` | Replies with only the OP.GG link for the current user. |
| `opgg multi` | Replies with the OP.GG multi-search link for the current lobby. Also accepts lobby/multi OP.GG phrases. |

### Development Commands

| Command | What it does |
| --- | --- |
| `bun install` | Installs frontend and Tauri JavaScript dependencies. |
| `bun run check` | Runs SvelteKit sync and Svelte diagnostics. |
| `bun run build` | Builds the SvelteKit frontend into `build/`. |
| `bun run tauri dev` | Starts the Tauri desktop app in development mode. |
| `bun tauri build` | Builds the production Tauri app and Windows installers. |
| `cargo check --manifest-path src-tauri/Cargo.toml` | Checks the Rust/Tauri backend without producing installers. |

### Release Commands

| Command | What it does |
| --- | --- |
| `gh secret set TAURI_SIGNING_PRIVATE_KEY --body (Get-Content "$env:USERPROFILE\.tauri\ghosty-updater.key" -Raw)` | Adds the Tauri updater private key to GitHub Secrets. Run once per repository/key. |
| `gh secret set TAURI_SIGNING_PRIVATE_KEY_PASSWORD --body ((Get-Content "$env:USERPROFILE\.tauri\ghosty-updater.key.password.txt" -Raw).Trim())` | Adds the Tauri updater private key password to GitHub Secrets. |
| `git tag app-v0.1.0` | Creates a release tag. Replace `0.1.0` with the app version. |
| `git push origin app-v0.1.0` | Pushes the release tag and starts the GitHub release workflow. |
