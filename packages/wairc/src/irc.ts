/**
 * irc.ts — singleton AircClient wrapper + reactive state for wairc.
 *
 * Keeps Preact components decoupled from the client internals.
 * All mutable state lives here; components subscribe via callbacks.
 */

import { AircClient, type ChannelMessage, type IrcEvent } from '@marlinski/airc'

// ─── types ────────────────────────────────────────────────────────────────────

export interface BufferLine {
  id: number
  ts: number       // unix seconds
  kind: 'msg' | 'action' | 'join' | 'part' | 'quit' | 'kick' | 'nick'
       | 'topic' | 'notice' | 'system' | 'error' | 'motd'
  from?: string
  text: string
  self?: boolean   // true when from === our nick
}

export interface ChannelEntry {
  name: string
  unread: number
  members: number
  memberList: string[]   // sorted nick list
  topic?: string
  lines: BufferLine[]
}

export interface IrcState {
  status: 'disconnected' | 'connecting' | 'connected'
  nick: string
  server: string
  channels: ChannelEntry[]
  activeChannel: string | null  // null = server buffer
  serverLines: BufferLine[]     // server / status buffer
  motd: string[]
  error?: string
}

export type StateListener = (state: IrcState) => void

// ─── internal counter ─────────────────────────────────────────────────────────

let _lineId = 0
function lineId() { return ++_lineId }

function fmtTime(ts: number): string {
  const d = new Date(ts * 1000)
  return d.toLocaleTimeString('en-GB', { hour: '2-digit', minute: '2-digit', second: '2-digit' })
}
export { fmtTime }

// ─── nick colour ──────────────────────────────────────────────────────────────

// Terminal-palette colours that read well on a dark background.
// Excludes red (reserved for errors) and yellow (reserved for self).
const NICK_COLOURS = [
  '#7ec8e3', // cyan
  '#5a9e5a', // green
  '#b07bd4', // purple
  '#d4a855', // orange
  '#6a9fbf', // steel blue
  '#c07ab8', // pink
  '#7ec87e', // light green
  '#d47b7b', // salmon
  '#7bbcd4', // sky blue
  '#a0c080', // olive
  '#d4c07b', // gold
  '#7bd4c0', // teal
]

export function nickColor(nick: string): string {
  let hash = 0
  for (let i = 0; i < nick.length; i++) {
    hash = (hash * 31 + nick.charCodeAt(i)) >>> 0
  }
  return NICK_COLOURS[hash % NICK_COLOURS.length]
}

// ─── command aliases ──────────────────────────────────────────────────────────

const ALIASES: Record<string, string> = {
  J:    'JOIN',
  WC:   'PART',
  PART: 'PART',
  Q:    'QUIT',
  M:    'MSG',
  W:    'WHOIS',
  T:    'TOPIC',
}

// ─── IrcManager ───────────────────────────────────────────────────────────────

class IrcManager {
  private _client: AircClient | null = null
  private _state: IrcState = {
    status: 'disconnected',
    nick: '',
    server: '',
    channels: [],
    activeChannel: null,
    serverLines: [],
    motd: [],
  }
  private _listeners: Set<StateListener> = new Set()

  // ── subscriptions ──────────────────────────────────────────────────────────

  subscribe(fn: StateListener) { this._listeners.add(fn) }
  unsubscribe(fn: StateListener) { this._listeners.delete(fn) }

  getState(): IrcState { return this._state }

  private _notify() {
    const s = { ...this._state }
    this._listeners.forEach(fn => fn(s))
  }

  private _setState(patch: Partial<IrcState>) {
    this._state = { ...this._state, ...patch }
    this._notify()
  }

  // ── buffer helpers ─────────────────────────────────────────────────────────

  private _pushServer(line: Omit<BufferLine, 'id'>) {
    const lines = [...this._state.serverLines, { ...line, id: lineId() }].slice(-2000)
    this._setState({ serverLines: lines })
  }

  private _pushChannel(name: string, line: Omit<BufferLine, 'id'>) {
    const lname = name.toLowerCase()
    const channels = this._state.channels.map(ch => {
      if (ch.name.toLowerCase() !== lname) return ch
      const isActive = this._state.activeChannel?.toLowerCase() === lname
      return {
        ...ch,
        lines: [...ch.lines, { ...line, id: lineId() }].slice(-2000),
        unread: isActive ? 0 : ch.unread + (line.kind === 'msg' || line.kind === 'action' ? 1 : 0),
      }
    })
    this._setState({ channels })
  }

  private _updateMembers(channel: string, fn: (list: string[]) => string[]) {
    const lname = channel.toLowerCase()
    const channels = this._state.channels.map(ch => {
      if (ch.name.toLowerCase() !== lname) return ch
      const memberList = fn([...ch.memberList]).sort((a, b) =>
        a.replace(/^[^a-zA-Z0-9]/, '').localeCompare(b.replace(/^[^a-zA-Z0-9]/, ''), undefined, { sensitivity: 'base' })
      )
      return { ...ch, memberList, members: memberList.length }
    })
    this._setState({ channels })
  }

  private _ensureChannel(name: string) {
    const lname = name.toLowerCase()
    if (!this._state.channels.find(c => c.name.toLowerCase() === lname)) {
      this._setState({
        channels: [...this._state.channels, { name, unread: 0, members: 0, memberList: [], lines: [] }],
      })
    }
  }

  private _removeChannel(name: string) {
    const lname = name.toLowerCase()
    const channels = this._state.channels.filter(c => c.name.toLowerCase() !== lname)
    const activeChannel =
      this._state.activeChannel?.toLowerCase() === lname
        ? (channels[0]?.name ?? null)
        : this._state.activeChannel
    this._setState({ channels, activeChannel })
  }

  // ── public API ─────────────────────────────────────────────────────────────

  setActive(channel: string | null) {
    const lname = channel?.toLowerCase() ?? null
    // clear unread when switching in
    const channels = this._state.channels.map(ch =>
      ch.name.toLowerCase() === lname ? { ...ch, unread: 0 } : ch
    )
    this._setState({ activeChannel: channel, channels })
  }

  async connect(opts: {
    url?: string
    nick: string
    password?: string
    saslAccount?: string
    autoJoin?: string[]
  }) {
    if (this._client) {
      this._client.destroy()
      this._client = null
    }
    this._setState({
      status: 'connecting',
      nick: opts.nick,
      server: opts.url ?? 'wss://irc.openlore.xyz/ws',
      channels: [],
      activeChannel: null,
      serverLines: [],
      motd: [],
      error: undefined,
    })

    this._client = new AircClient(
      {
        url: opts.url,
        nick: opts.nick,
        password: opts.password,
        saslAccount: opts.saslAccount,
        autoJoin: opts.autoJoin,
        // Disable echo-message: we display sent messages optimistically in
        // send(), so we don't want the server to echo them back to us.
        disableCaps: ['echo-message'],
      },
      { initialDelay: 2000, maxDelay: 30000, backoffFactor: 2 }
    )

    this._client.on(this._handleEvent)

    try {
      const { motd } = await this._client.connect()
      this._setState({ status: 'connected', nick: this._client.nick(), motd })
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err)
      this._setState({ status: 'disconnected', error: msg })
    }
  }

  disconnect() {
    this._client?.quit('leaving')
    this._client = null
    this._setState({ status: 'disconnected' })
  }

  send(text: string) {
    if (!this._client) return
    const active = this._state.activeChannel
    if (!active) return

    if (text.startsWith('/')) {
      this._handleCommand(text)
      return
    }

    this._client.say(active, text)
    const ts = Math.floor(Date.now() / 1000)
    const nick = this._state.nick
    this._pushChannel(active, { ts, kind: 'msg', from: nick, text, self: true })
  }

  private _handleCommand(raw: string) {
    const parts = raw.slice(1).split(' ')
    const cmd = parts[0].toUpperCase()
    const args = parts.slice(1)

    // resolve aliases
    const resolved = ALIASES[cmd] ?? cmd

    switch (resolved) {
      case 'JOIN': {
        const ch = args[0]
        if (ch) this._client?.join(ch)
        break
      }
      case 'PART': {
        const ch = args[0]?.startsWith('#') ? args[0] : (this._state.activeChannel ?? undefined)
        const reason = args[0]?.startsWith('#') ? args.slice(1).join(' ') : args.join(' ')
        if (ch) this._client?.part(ch, reason || 'leaving')
        break
      }
      case 'NICK': {
        const n = args[0]
        if (n) this._client?.sendLine(`NICK ${n}`)
        break
      }
      case 'MSG':
      case 'PRIVMSG': {
        const target = args[0]
        const text = args.slice(1).join(' ')
        if (target && text) this._client?.say(target, text)
        break
      }
      case 'ME': {
        const ch = this._state.activeChannel
        const text = args.join(' ')
        if (ch && text) {
          this._client?.say(ch, `\x01ACTION ${text}\x01`)
          const ts = Math.floor(Date.now() / 1000)
          this._pushChannel(ch, { ts, kind: 'action', from: this._state.nick, text, self: true })
        }
        break
      }
      case 'QUIT': {
        this.disconnect()
        break
      }
      case 'TOPIC': {
        const ch = args[0] ?? this._state.activeChannel
        const topic = args.slice(ch === args[0] ? 1 : 0).join(' ')
        if (ch) this._client?.sendLine(topic ? `TOPIC ${ch} :${topic}` : `TOPIC ${ch}`)
        break
      }
      case 'RAW': {
        this._client?.sendLine(args.join(' '))
        break
      }
      case 'WHOIS': {
        const target = args[0]
        if (target) this._client?.sendLine(`WHOIS ${target}`)
        break
      }
      default: {
        const ts = Math.floor(Date.now() / 1000)
        this._pushServer({ ts, kind: 'error', text: `Unknown command: /${resolved}` })
      }
    }
  }

  // ── event handler ──────────────────────────────────────────────────────────

  private _handleEvent = (ev: IrcEvent) => {
    const ts = Math.floor(Date.now() / 1000)

    switch (ev.type) {
      case 'registered':
        this._setState({ status: 'connected', nick: ev.nick, server: ev.server })
        this._pushServer({ ts, kind: 'system', text: `Connected to ${ev.server} as ${ev.nick}` })
        break

      case 'message': {
        const m: ChannelMessage = ev.message
        const isAction = m.text.startsWith('\x01ACTION ') && m.text.endsWith('\x01')
        const text = isAction ? m.text.slice(8, -1) : m.text
        const kind = isAction ? 'action' : 'msg'
        this._ensureChannel(m.target)
        this._pushChannel(m.target, { ts: m.timestamp, kind, from: m.from, text })
        break
      }

      case 'join': {
        this._ensureChannel(ev.channel)
        const isSelf = ev.nick === this._state.nick
        if (isSelf) {
          this._setState({ activeChannel: ev.channel })
        }
        this._updateMembers(ev.channel, list => {
          if (!list.includes(ev.nick)) list.push(ev.nick)
          return list
        })
        this._pushChannel(ev.channel, {
          ts, kind: 'join', from: ev.nick,
          text: isSelf ? `You have joined ${ev.channel}` : `${ev.nick} has joined`,
        })
        break
      }

      case 'part': {
        const isSelf = ev.nick === this._state.nick
        this._updateMembers(ev.channel, list => list.filter(n => n !== ev.nick))
        this._pushChannel(ev.channel, {
          ts, kind: 'part', from: ev.nick,
          text: isSelf
            ? `You have left ${ev.channel}${ev.reason ? ` (${ev.reason})` : ''}`
            : `${ev.nick} has left${ev.reason ? ` (${ev.reason})` : ''}`,
        })
        if (isSelf) this._removeChannel(ev.channel)
        break
      }

      case 'quit': {
        // remove from all channels + push quit line
        this._state.channels.forEach(ch => {
          this._updateMembers(ch.name, list => list.filter(n => n !== ev.nick))
          this._pushChannel(ch.name, {
            ts, kind: 'quit', from: ev.nick,
            text: `${ev.nick} has quit${ev.reason ? ` (${ev.reason})` : ''}`,
          })
        })
        break
      }

      case 'kick':
        this._updateMembers(ev.channel, list => list.filter(n => n !== ev.nick))
        this._pushChannel(ev.channel, {
          ts, kind: 'part', from: ev.by,
          text: `${ev.nick} was kicked by ${ev.by}${ev.reason ? ` (${ev.reason})` : ''}`,
        })
        if (ev.nick === this._state.nick) this._removeChannel(ev.channel)
        break

      case 'topic_change': {
        const channels = this._state.channels.map(ch =>
          ch.name.toLowerCase() === ev.channel.toLowerCase()
            ? { ...ch, topic: ev.topic }
            : ch
        )
        this._setState({ channels })
        this._pushChannel(ev.channel, {
          ts, kind: 'topic', from: ev.setBy,
          text: `${ev.setBy} changed topic to: ${ev.topic}`,
        })
        break
      }

      case 'nick_change': {
        const isSelf = ev.oldNick === this._state.nick
        if (isSelf) this._setState({ nick: ev.newNick })
        this._state.channels.forEach(ch => {
          this._updateMembers(ch.name, list => {
            const idx = list.indexOf(ev.oldNick)
            if (idx !== -1) list[idx] = ev.newNick
            return list
          })
          this._pushChannel(ch.name, {
            ts, kind: 'nick', from: ev.oldNick,
            text: `${ev.oldNick} is now known as ${ev.newNick}`,
          })
        })
        break
      }

      case 'notice':
        this._pushServer({ ts, kind: 'notice', from: ev.from, text: ev.text })
        break

      case 'motd':
        this._pushServer({ ts, kind: 'motd', text: ev.line })
        break

      case 'motd_end':
        this._pushServer({ ts, kind: 'system', text: '--- end of motd ---' })
        break

      case 'sasl_logged_in':
        this._pushServer({ ts, kind: 'system', text: `SASL: logged in as ${ev.account}` })
        break

      case 'sasl_failed':
        this._pushServer({ ts, kind: 'error', text: `SASL failed: ${ev.reason}` })
        break

      case 'disconnected':
        this._setState({ status: 'disconnected' })
        this._pushServer({ ts, kind: 'error', text: `Disconnected: ${ev.reason}` })
        break

      case 'reconnecting':
        this._setState({ status: 'connecting' })
        this._pushServer({ ts, kind: 'system', text: `Reconnecting... (attempt ${ev.attempt})` })
        break

      case 'reconnected':
        this._setState({ status: 'connected' })
        this._pushServer({ ts, kind: 'system', text: 'Reconnected.' })
        break

      case 'names': {
        this._ensureChannel(ev.channel)
        this._updateMembers(ev.channel, list => {
          const merged = new Set([...list, ...ev.members])
          return Array.from(merged)
        })
        break
      }

      case 'topic': {
        const channels = this._state.channels.map(ch =>
          ch.name.toLowerCase() === ev.channel.toLowerCase()
            ? { ...ch, topic: ev.topic }
            : ch
        )
        this._setState({ channels })
        break
      }

      case 'raw':
        // no-op: library surfaces all structured events; raw is a last-resort escape hatch
        break
    }
  }
}

export const irc = new IrcManager()
