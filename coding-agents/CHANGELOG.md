# Changelog

## 1.2.0

- **Claude Limits** and **Codex Limits** send their numbers as the tile's value: the title is yours, and `{0}` (or `%0%`) in it shows where the usage goes (`5 Hour {0}`). A new key starts as `Claude {0}` / `Codex {0}`; an empty title shows the usage alone. Older KnobDeck versions still get the numbers as the title.

## 1.1.0

- **Claude Limits** and **Codex Limits** keys (5-hour, weekly and, for Claude, Fable gauges), replacing the AI Stats plugin.
- No more rate limiting: the numbers come from Claude Code's status line and Codex's session logs, with no network calls. The providers' usage endpoints are only a fallback when those numbers are over 15 minutes old, at most once per 10 minutes, with `Retry-After` and a growing back-off kept across restarts; the last good numbers stay on the key meanwhile.
- Setup can connect Claude Code's status line for this (on by default). A status line you already have keeps working and is restored when you untick it.

## 1.0.1

- A proper app icon in the Plugins store and sidebar (`icons/app-icon.svg`).

