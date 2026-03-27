/**
 * ConnectScreen.tsx — initial connection form, weechat /connect style
 */

import { useState } from 'preact/hooks'
import { irc } from '../irc'
import type { JSX } from 'preact'

interface Props {
  error?: string
}

export function ConnectScreen({ error }: Props) {
  const [nick, setNick] = useState('')
  const [password, setPassword] = useState('')
  const [channels, setChannels] = useState('#lobby')
  const [connecting, setConnecting] = useState(false)

  const submit = async (e: JSX.TargetedEvent<HTMLFormElement>) => {
    e.preventDefault()
    if (!nick.trim()) return
    setConnecting(true)
    const autoJoin = channels
      .split(',')
      .map(s => s.trim())
      .filter(s => s.startsWith('#'))
    await irc.connect({
      nick: nick.trim(),
      password: password.trim() || undefined,
      autoJoin,
    })
    setConnecting(false)
  }

  return (
    <div class="connect-screen">
      <div class="connect-box">
        <div class="connect-banner">
          <pre class="ascii-art">{BANNER}</pre>
          <p class="connect-sub">web irc client &mdash; airc suite</p>
        </div>

        <form class="connect-form" onSubmit={submit}>
          <div class="field">
            <label for="f-nick">nick</label>
            <input
              id="f-nick"
              type="text"
              value={nick}
              onInput={e => setNick((e.target as HTMLInputElement).value)}
              placeholder="yournick"
              spellcheck={false}
              autocomplete="off"
              required
            />
          </div>

          <div class="field">
            <label for="f-pass">password</label>
            <input
              id="f-pass"
              type="password"
              value={password}
              onInput={e => setPassword((e.target as HTMLInputElement).value)}
              placeholder="(optional)"
              autocomplete="current-password"
            />
          </div>

          <div class="field">
            <label for="f-chans">channels</label>
            <input
              id="f-chans"
              type="text"
              value={channels}
              onInput={e => setChannels((e.target as HTMLInputElement).value)}
              placeholder="#lobby,#dev"
              spellcheck={false}
              autocomplete="off"
            />
          </div>

          {error && <div class="connect-error">{error}</div>}

          <button type="submit" class="connect-btn" disabled={connecting}>
            {connecting ? 'connecting...' : '/connect'}
          </button>
        </form>

        <div class="connect-hint">
          type <span class="kbd">/join #channel</span> after connecting
        </div>
      </div>
    </div>
  )
}

const BANNER = `
 ██╗    ██╗ █████╗ ██╗██████╗  ██████╗
 ██║    ██║██╔══██╗██║██╔══██╗██╔════╝
 ██║ █╗ ██║███████║██║██████╔╝██║
 ██║███╗██║██╔══██║██║██╔══██╗██║
 ╚███╔███╔╝██║  ██║██║██║  ██║╚██████╗
  ╚══╝╚══╝ ╚═╝  ╚═╝╚═╝╚═╝  ╚═╝ ╚═════╝`;
