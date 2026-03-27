/**
 * Buffer.tsx — message/log area, auto-scrolls to bottom
 */

import { useEffect, useRef } from 'preact/hooks'
import { type IrcState, type BufferLine, fmtTime, nickColor } from '../irc'

interface Props {
  state: IrcState
}

export function Buffer({ state }: Props) {
  const { activeChannel, channels, serverLines, nick } = state
  const ref = useRef<HTMLDivElement>(null)

  const ch = activeChannel
    ? channels.find(c => c.name.toLowerCase() === activeChannel.toLowerCase())
    : null

  const lines: BufferLine[] = ch ? ch.lines : serverLines

  // auto-scroll to bottom on new lines
  useEffect(() => {
    const el = ref.current
    if (!el) return
    // only auto-scroll if already near the bottom
    const nearBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 120
    if (nearBottom) el.scrollTop = el.scrollHeight
  }, [lines.length])

  // force scroll on channel switch
  useEffect(() => {
    if (ref.current) ref.current.scrollTop = ref.current.scrollHeight
  }, [activeChannel])

  return (
    <div class="buffer-wrap">
      {ch?.topic && (
        <div class="buffer-topic">
          <span class="topic-label">topic:</span> {ch.topic}
        </div>
      )}
      <div class="buffer" ref={ref}>
        {lines.length === 0 && (
          <div class="buffer-empty">
            {activeChannel ? `${activeChannel} — no messages yet` : 'status — nothing here yet'}
          </div>
        )}
        {lines.map(line => (
          <Line key={line.id} line={line} selfNick={nick} />
        ))}
      </div>
    </div>
  )
}

// ─── individual line ──────────────────────────────────────────────────────────

function Line({ line, selfNick }: { line: BufferLine; selfNick: string }) {
  const time = fmtTime(line.ts)
  const isSelf = line.from === selfNick

  switch (line.kind) {
    case 'msg':
      return (
        <div class={`line line-msg ${isSelf ? 'line-self' : ''}`}>
          <span class="line-ts">{time}</span>
          <span class="line-bracket">&lt;</span>
          <span class="line-nick" style={isSelf ? undefined : { color: nickColor(line.from ?? '') }}>{line.from}</span>
          <span class="line-bracket">&gt;</span>
          <span class="line-text">{line.text}</span>
        </div>
      )

    case 'action':
      return (
        <div class={`line line-action ${isSelf ? 'line-self' : ''}`}>
          <span class="line-ts">{time}</span>
          <span class="line-star">*</span>
          <span class="line-nick" style={isSelf ? undefined : { color: nickColor(line.from ?? '') }}>{line.from}</span>
          <span class="line-text">{line.text}</span>
        </div>
      )

    case 'notice':
      return (
        <div class="line line-notice">
          <span class="line-ts">{time}</span>
          <span class="line-notice-prefix">-{line.from ?? '*'}-</span>
          <span class="line-text">{line.text}</span>
        </div>
      )

    case 'join':
      return (
        <div class="line line-join">
          <span class="line-ts">{time}</span>
          <span class="line-event">--&gt;</span>
          <span class="line-text">{line.text}</span>
        </div>
      )

    case 'part':
    case 'quit':
      return (
        <div class="line line-part">
          <span class="line-ts">{time}</span>
          <span class="line-event">&lt;--</span>
          <span class="line-text">{line.text}</span>
        </div>
      )

    case 'kick':
      return (
        <div class="line line-part">
          <span class="line-ts">{time}</span>
          <span class="line-event">&lt;-!</span>
          <span class="line-text">{line.text}</span>
        </div>
      )

    case 'nick':
      return (
        <div class="line line-system">
          <span class="line-ts">{time}</span>
          <span class="line-event">---</span>
          <span class="line-text">{line.text}</span>
        </div>
      )

    case 'topic':
      return (
        <div class="line line-topic">
          <span class="line-ts">{time}</span>
          <span class="line-event">---</span>
          <span class="line-text">{line.text}</span>
        </div>
      )

    case 'motd':
      return (
        <div class="line line-motd">
          <span class="line-ts">{time}</span>
          <span class="line-motd-prefix">motd</span>
          <span class="line-text">{line.text}</span>
        </div>
      )

    case 'system':
      return (
        <div class="line line-system">
          <span class="line-ts">{time}</span>
          <span class="line-event">***</span>
          <span class="line-text">{line.text}</span>
        </div>
      )

    case 'error':
      return (
        <div class="line line-error">
          <span class="line-ts">{time}</span>
          <span class="line-event">!!!</span>
          <span class="line-text">{line.text}</span>
        </div>
      )

    default:
      return (
        <div class="line line-system">
          <span class="line-ts">{time}</span>
          <span class="line-text">{line.text}</span>
        </div>
      )
  }
}
