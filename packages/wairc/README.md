# wairc

A minimal, terminal-aesthetic web IRC client — weechat/irssi-style in the browser.

Part of the [airc suite](../../README.md).

## Features

- Weechat/irssi-inspired UI: monospace font, dark terminal palette
- Channel sidebar with unread counts
- Message buffer with auto-scroll
- Full-featured input: IRC commands, arrow-key history
- Built on [`@marlinski/airc`](https://www.npmjs.com/package/@marlinski/airc) — connects over WebSocket to aircd
- Auto-reconnect with exponential backoff

## Supported commands

| Command | Description |
|---|---|
| `/join #channel` | Join a channel |
| `/part [#channel] [reason]` | Leave a channel |
| `/nick <newnick>` | Change nickname |
| `/msg <target> <text>` | Send a private message |
| `/me <text>` | Send an action (`/me waves`) |
| `/topic [#channel] [text]` | Get or set topic |
| `/raw <line>` | Send a raw IRC line |
| `/quit` | Disconnect |

## Development

```bash
npm install
npm run dev
```

## Build

```bash
npm run build
# output in dist/
```

## Docker

```bash
docker build -t wairc .
docker run -p 8080:80 wairc
```

The image serves the static build via nginx on port 80.

## Configuration

Connection settings (server URL, nick, password, channels) are entered at the connect screen. The default server is `wss://irc.openlore.xyz/ws`.
