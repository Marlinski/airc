/**
 * app.tsx — top-level layout
 *
 * Layout (when connected):
 *   ┌──────────┬─────────────────────────────┬──────────┐
 *   │ sidebar  │  buffer (messages)           │ members  │
 *   │          │                              │          │
 *   │          ├──────────────────────────────┤          │
 *   │          │  input bar                   │          │
 *   └──────────┴──────────────────────────────┴──────────┘
 */

import { useState, useEffect } from 'preact/hooks'
import { irc, type IrcState } from './irc'
import { ConnectScreen } from './components/ConnectScreen'
import { Sidebar } from './components/Sidebar'
import { Buffer } from './components/Buffer'
import { InputBar } from './components/InputBar'
import { MemberList } from './components/MemberList'

export function App() {
  const [state, setState] = useState<IrcState>(irc.getState())

  useEffect(() => {
    irc.subscribe(setState)
    return () => irc.unsubscribe(setState)
  }, [])

  if (state.status === 'disconnected' && state.nick === '') {
    return <ConnectScreen error={state.error} />
  }

  if (state.status === 'connecting' && state.channels.length === 0 && state.serverLines.length === 0) {
    return (
      <div class="splash">
        <span class="splash-text">connecting to {state.server}...</span>
      </div>
    )
  }

  if (state.status === 'disconnected') {
    return <ConnectScreen error={state.error ?? 'Disconnected.'} />
  }

  const inChannel = state.activeChannel !== null

  return (
    <div class="layout">
      <Sidebar state={state} />
      <div class="main">
        <Buffer state={state} />
        <InputBar state={state} />
      </div>
      {inChannel && <MemberList state={state} />}
    </div>
  )
}
