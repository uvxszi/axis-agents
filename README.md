# Axis Bot Studio

A small desktop app (single `.exe`) that runs a Discord bot built on the
[Axis](../README.md) library — paste your token, set up slash commands,
and watch the gateway live. No console, no code changes needed for
command tweaks.

![layout](https://img.shields.io/badge/built%20with-eframe%20%2B%20axis-blue)

## Build

```sh
cd app
cargo build --release
```

The binary lands at `app/target/release/axis_bot_studio.exe` (~11 MB).
This crate is a separate workspace on purpose: the GUI stack (eframe)
needs windowing libraries the library CI does not.

## Running

1. Create a bot at the [Discord Developer
   Portal](https://discord.com/developers/applications) and copy the
   **bot token** and the **application ID** (General Information →
   Application ID).
2. In **Bot → Privileged Gateway Intents**, enable **Message Content
   Intent** (required for reading message text; the app requests it).
3. Invite the bot to your server (OAuth2 → URL Generator → `bot` +
   `applications.commands` scopes).
4. Start `axis_bot_studio.exe`, paste the token, enter the application
   ID, press **Connect**.

The token is **never written to disk** — it lives only in memory for the
session. The application ID and your command definitions are saved to
`axis_bot_studio.json` next to the executable.

## Features

- **Connect / Disconnect** with live status: connecting → online →
  resumed → reconnecting (with close codes) → disconnected. A bad token
  surfaces immediately as a failure instead of hanging.
- **Command setup**: add, edit, and remove slash commands with options
  (text / number / true-false, required or not). Commands are reply
  templates — `{option_name}` and `{user}` placeholders are substituted
  per invocation. Declaration mistakes (bad names, ordering) are caught
  locally before Discord ever sees them.
- **Sync to Discord**: one button performs a global bulk-overwrite of
  the command set through the library's rate-limit-aware REST client.
  Built-ins are always included: `/ping`, `/echo`, `/roll`.
- **Live log**: every gateway lifecycle change, command invocation, and
  error, timestamped.
- **Messages tab**: the most recent 200 messages the bot has seen.
- **Send message**: push a message to any channel by ID through the
  bot's REST client.

## How it works

The GUI (eframe) and the bot (Tokio) run on separate threads, bridged by
channels: the UI sends commands (`Connect`, `SendMessage`,
`SyncCommands`), the supervisor streams events back (`Status`,
`Message`, `Log`). The command registry lives behind a shared lock, so
command edits apply to the next interaction without reconnecting. All of
the Discord plumbing is the Axis library: gateway shard with
resume/reconnect, typed REST client with rate limiting, and the command
framework.
