/**
 * Sidebar.tsx — channel list, weechat left-column style
 */

import { irc, type IrcState } from '../irc'

interface Props {
  state: IrcState
}

export function Sidebar({ state }: Props) {
  const { channels, activeChannel, nick, status, serverLines } = state

  const statusDot = status === 'connected'
    ? <span class="dot dot-ok" title="connected" />
    : status === 'connecting'
    ? <span class="dot dot-warn" title="connecting" />
    : <span class="dot dot-err" title="disconnected" />

  // server buffer unread = 0 (always shown, focus is channel)
  const serverActive = activeChannel === null

  return (
    <aside class="sidebar">
      <div class="sidebar-header">
        {statusDot}
        <span class="sidebar-nick">{nick || '—'}</span>
      </div>

      <div class="sidebar-section">
        <div
          class={`sidebar-item sidebar-server ${serverActive ? 'active' : ''}`}
          onClick={() => irc.setActive(null)}
        >
          <span class="sidebar-item-name">status</span>
          {serverLines.length > 0 && !serverActive && (
            <span class="sidebar-unread">!</span>
          )}
        </div>
      </div>

      {channels.length > 0 && (
        <div class="sidebar-section">
          <div class="sidebar-section-label">channels</div>
          {channels.map(ch => {
            const isActive = activeChannel?.toLowerCase() === ch.name.toLowerCase()
            return (
              <div
                key={ch.name}
                class={`sidebar-item ${isActive ? 'active' : ''} ${ch.unread > 0 ? 'has-unread' : ''}`}
                onClick={() => irc.setActive(ch.name)}
              >
                <span class="sidebar-item-name">{ch.name}</span>
                {ch.unread > 0 && !isActive && (
                  <span class="sidebar-unread">{ch.unread > 99 ? '99+' : ch.unread}</span>
                )}
              </div>
            )
          })}
        </div>
      )}

      <div class="sidebar-footer">
        <span
          class="sidebar-quit"
          onClick={() => irc.disconnect()}
          title="/quit"
        >
          [disconnect]
        </span>
      </div>
    </aside>
  )
}
