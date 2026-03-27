/**
 * InputBar.tsx — command/message input, weechat style
 */

import { useState, useRef } from 'preact/hooks'
import { irc, type IrcState } from '../irc'
import type { JSX } from 'preact'

interface Props {
  state: IrcState
}

const HISTORY_MAX = 200

export function InputBar({ state }: Props) {
  const { activeChannel, nick, status } = state
  const [text, setText] = useState('')
  const [history, setHistory] = useState<string[]>([])
  const [histIdx, setHistIdx] = useState(-1)
  const [saved, setSaved] = useState('')   // buffer saved when browsing history
  const inputRef = useRef<HTMLInputElement>(null)

  const target = activeChannel ?? 'status'
  const prompt = activeChannel ? `${nick}:${activeChannel}` : `${nick}:status`
  const disabled = status !== 'connected'

  const submit = (e: JSX.TargetedEvent<HTMLFormElement>) => {
    e.preventDefault()
    const line = text.trim()
    if (!line) return

    irc.send(line)
    setHistory(prev => [line, ...prev].slice(0, HISTORY_MAX))
    setHistIdx(-1)
    setSaved('')
    setText('')
  }

  const onKeyDown = (e: KeyboardEvent) => {
    if (e.key === 'ArrowUp') {
      e.preventDefault()
      if (history.length === 0) return
      if (histIdx === -1) setSaved(text)
      const next = Math.min(histIdx + 1, history.length - 1)
      setHistIdx(next)
      setText(history[next])
    } else if (e.key === 'ArrowDown') {
      e.preventDefault()
      if (histIdx === -1) return
      const next = histIdx - 1
      if (next < 0) {
        setHistIdx(-1)
        setText(saved)
      } else {
        setHistIdx(next)
        setText(history[next])
      }
    }
  }

  return (
    <form class="inputbar" onSubmit={submit}>
      <span class="inputbar-prompt">{prompt}&gt;</span>
      <input
        ref={inputRef}
        class="inputbar-input"
        type="text"
        value={text}
        onInput={e => setText((e.target as HTMLInputElement).value)}
        onKeyDown={onKeyDown}
        disabled={disabled}
        placeholder={disabled ? '(not connected)' : `message ${target} — /join #channel /part /quit`}
        spellcheck={false}
        autocomplete="off"
        autocorrect="off"
        autocapitalize="off"
      />
    </form>
  )
}
