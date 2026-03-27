/**
 * MemberList.tsx — right-side panel showing channel members
 */

import { type IrcState, nickColor } from '../irc'

interface Props {
  state: IrcState
}

export function MemberList({ state }: Props) {
  const { activeChannel, channels } = state

  const ch = activeChannel
    ? channels.find(c => c.name.toLowerCase() === activeChannel.toLowerCase())
    : null

  if (!ch) return null

  const members = ch.memberList

  return (
    <aside class="memberlist">
      <div class="memberlist-header">
        <span class="memberlist-count">{members.length}</span>
        <span class="memberlist-label"> members</span>
      </div>
      <div class="memberlist-body">
        {members.map(nick => (
          <div key={nick} class="memberlist-nick">
            <span class="memberlist-sigil">{sigil(nick)}</span>
            <span class="memberlist-name" style={{ color: nickColor(stripSigil(nick)) }}>{stripSigil(nick)}</span>
          </div>
        ))}
        {members.length === 0 && (
          <div class="memberlist-empty">—</div>
        )}
      </div>
    </aside>
  )
}

// IRC mode prefixes: @ op, + voice, % halfop, & protected, ~ owner
const SIGILS = new Set(['~', '&', '@', '%', '+'])

function sigil(nick: string): string {
  return SIGILS.has(nick[0]) ? nick[0] : ' '
}

function stripSigil(nick: string): string {
  return SIGILS.has(nick[0]) ? nick.slice(1) : nick
}
