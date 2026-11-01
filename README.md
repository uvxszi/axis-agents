# Axis Agents

A small desktop app (single `.exe`) that runs an AI-powered Discord bot
built on the [Axis](../README.md) library — paste your token, set up
slash commands, configure an LLM agent with permission-gated tools, and
watch the gateway live.

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
- **AI Agent (Axis Agents)**: configure an LLM provider (LM Studio on
  localhost, OpenAI, OpenRouter, Groq, or custom), set the agent's name
  and standing instructions, and let it answer `/agent` commands and
  `@mentions` — with **MCP-style tools** (ban, kick, timeout, roles,
  messages, lookups) that are **strictly permission-gated** to the
  invoking user's real Discord permissions, enforced in code. A prompt
  injection cannot make the agent act beyond what the user may do.
- **Command setup**: add, edit, and remove slash commands with options
  (text / number / true-false, required or not) and three behaviors
  (reply template, dice roll, AI agent). Built-ins (`/ping`, `/echo`,
  `/roll`, `/agent`) are seeded into the list and fully editable.
  Declaration mistakes are caught locally before Discord ever sees them.
- **Sync to Discord**: one button performs a global bulk-overwrite of
  the command set through the library's rate-limit-aware REST client.
- **Live log**: every gateway lifecycle change, command invocation, and
  error, timestamped.
- **Messages tab**: the most recent 200 messages the bot has seen.
- **Send message**: push a message to any channel by ID through the
  bot's REST client.
- **Remember token** (optional, plain text next to the exe) — off by
  default.
- **Emoji support**: full-coverage Noto Emoji font is embedded, so
  message content renders correctly.

## How it works

The GUI (eframe) and the bot (Tokio) run on separate threads, bridged by
channels: the UI sends commands (`Connect`, `SendMessage`,
`SyncCommands`), the supervisor streams events back (`Status`,
`Message`, `Log`). The command registry lives behind a shared lock, so
command edits apply to the next interaction without reconnecting. All of
the Discord plumbing is the Axis library: gateway shard with
resume/reconnect, typed REST client with rate limiting, and the command
framework.
