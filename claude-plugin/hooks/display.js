import { CommandError, clean } from './data.js'

// What a command shows comes from the CLI: with --json --display it adds a
// `display`, the same blocks its terminal output is drawn from
// (switchboard/src/cli/view.rs). This turns them into this plugin's layout
// document (layout.js), so both surfaces say the same thing in the same order.
// tests/display-fixture.js holds every display the CLI's tests check.

// The commands this plugin has: `switchboard X` reads `/switchboard X` in a chat.
export const SLASH_COMMANDS = ['status', 'chats', 'join', 'leave', 'ping', 'up', 'down', 'doctor', 'help']

const KINDS = new Set(['title', 'heading', 'fact', 'text', 'next', 'item', 'table', 'code'])
const TONES = new Set(['success', 'warning', 'error'])

// A command as a chat runs it.
export function inChat(command) {
  const text = clean(command, 512)
  const match = /^switchboard ([a-z]+)\b(.*)$/.exec(text)
  return match && SLASH_COMMANDS.includes(match[1]) ? '/switchboard ' + match[1] + match[2] : text
}

// Text with its `quoted` commands as a chat runs them, without the backticks.
export function words(value, limit = 1024) {
  if (typeof value !== 'string') return ''
  return clean(value.replace(/`([^`]*)`/g, (_, command) => inChat(command)), limit)
}

// The CLI writes a fix as a sentence ("Run switchboard up."); after "To fix:"
// it reads on in lowercase, except for names and abbreviations.
const NAMES = new Set(['Switchboard', 'Claude', 'Caddy', 'Tailscale', 'Linux', 'Python', 'Homebrew', 'GitHub', 'I'])
export function continued(sentence) {
  const word = sentence.split(/[\s.,;:!?]/, 1)[0]
  return /^[A-Z][a-z]*$/.test(word) && !NAMES.has(word) ? sentence.charAt(0).toLowerCase() + sentence.slice(1) : sentence
}

function invalid() {
  return new CommandError('Switchboard returned an invalid display.')
}

function strings(value, limit, count) {
  if (!Array.isArray(value) || value.length > count || value.some(item => typeof item !== 'string')) throw invalid()
  return value.map(item => clean(item, limit))
}

// The layout document for a CLI display; anything unexpected is refused, not shown.
export function fromDisplay(display) {
  if (!Array.isArray(display) || !display.length || display.length > 600) throw invalid()
  const doc = []
  for (const block of display) {
    if (!block || typeof block !== 'object' || Array.isArray(block) || !KINDS.has(block.kind)) throw invalid()
    const space = block.space === true ? { space: true } : {}
    const tone = TONES.has(block.tone) ? { tone: block.tone } : {}
    if (block.kind === 'title' || block.kind === 'heading') doc.push({ kind: block.kind, text: words(block.text), ...tone, ...space })
    else if (block.kind === 'fact') doc.push({ kind: 'fact', label: clean(block.label, 64), value: words(block.value), ...space })
    else if (block.kind === 'text') doc.push({ kind: 'text', text: words(block.text), ...tone, ...(block.strong === true ? { strong: true } : {}), ...space })
    else if (block.kind === 'next') doc.push({ kind: 'text', text: words(block.text), ...space }, { kind: 'code', text: inChat(block.command) })
    else if (block.kind === 'code') doc.push({ kind: 'code', text: inChat(block.text), ...space })
    else if (block.kind === 'item') {
      const fix = words(block.fix)
      doc.push({ kind: 'item', text: words(block.text), ...tone, ...(fix ? { note: 'To fix: ' + continued(fix) } : {}), ...space })
    } else {
      const columns = strings(block.columns, 64, 8)
      if (!columns.length || !Array.isArray(block.rows) || block.rows.length > 200) throw invalid()
      const rows = block.rows.map(row => {
        if (!row || typeof row !== 'object') throw invalid()
        const cells = strings(row.cells, 256, columns.length)
        if (row.note !== undefined && typeof row.note !== 'string') throw invalid()
        return { cells, ...(row.note ? { note: clean(row.note, 64) } : {}) }
      })
      doc.push({ kind: 'table', columns, rows, ...space })
    }
  }
  return doc
}

// The plugin's help, as the CLI's `help --json --display` gives it (the help
// works without the CLI, so it is written here; the tests check it is the same).
export const HELP_DISPLAY = [
  { kind: 'title', text: '/switchboard [command]' },
  { kind: 'table', space: true, columns: ['Command', 'What it does'], rows: [
    { cells: ['status', 'Whether this machine is connected, and this chat'] },
    { cells: ['chats', 'List chats on this machine and others'] },
    { cells: ['join [name]', 'Join this chat, so chats on other machines can message it'] },
    { cells: ['leave', 'Leave this chat; other machines can no longer see it'] },
    { cells: ['ping <chat>', 'Check that a chat on another machine is reachable'] },
    { cells: ['up', 'Bring this machine back online'] },
    { cells: ['down', 'Take this machine offline until you bring it back up'] },
    { cells: ['doctor', 'Check everything, with the fix for each problem'] },
    { cells: ['help', 'Show these commands'] },
  ] },
  { kind: 'heading', space: true, text: 'Examples' },
  { kind: 'code', text: '/switchboard join release-planner' },
  { kind: 'code', text: '/switchboard chats --remote research' },
  { kind: 'code', text: '/switchboard chats --details project-name' },
  { kind: 'code', text: '/switchboard ping laptop/research' },
  { kind: 'text', space: true, text: 'Chats takes --local or --remote, --details, and search words that match names, IDs, machines, projects, repositories, and branches.' },
  { kind: 'text', text: 'A joined chat leaves after 7 days without work or messages. Closing or resuming a chat does not count.' },
  { kind: 'text', text: 'To add a machine, make an invite on the server with `sudo switchboard server invite`, then run `switchboard init` on the new machine.' },
]

// What status shows on a machine that isn't set up (the CLI reports it as an
// error in JSON, and shows this in a terminal).
export const NOT_SET_UP_DISPLAY = [{ kind: 'title', text: 'Not set up. Run `switchboard init`.' }]
