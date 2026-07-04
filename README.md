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
