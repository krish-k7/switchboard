import { CommandError, activity, clean, validSession } from './data.js'
import { HELP_DISPLAY, continued, fromDisplay } from './display.js'

// Every formatter returns a layout document (see layout.js), never styled text.
// The formatters below draw answers from a CLI older than 0.3, which has no
// display of its own (see display.js for what newer CLIs send).

// The help: the same commands, words, and examples as the CLI's.
export const HELP = fromDisplay(HELP_DISPLAY)

// Machines join with invites made on the server, which also lists and revokes them.
const MOVED = 'Machines are managed on the server now. To add one, make an invite there with sudo switchboard server invite, then run switchboard init on the new machine. List and remove machines on the server with sudo switchboard server machines and sudo switchboard server revoke NAME.'

// `code` is an optional command shown on its own line, for example how to install.
// A CLI message as a sentence: capital first letter, closing punctuation.
export function sentence(text) {
  return text.charAt(0).toUpperCase() + text.slice(1) + (/[.!?]$/.test(text) ? '' : '.')
}

export function errorDoc(message, code = '') {
  return [{ kind: 'text', tone: 'error', text: 'Error: ' + message }, ...(code ? [{ kind: 'code', text: code }] : [])]
}

export function parseArgs(text) {
  const tokens = text.trim().split(/\s+/).filter(Boolean)
  if (!tokens.length) return { command: 'status', overview: true }
  const command = tokens.shift()
  if (['indicator', 'close'].includes(command)) return { command: 'removed' }
  if (['machines', 'approve', 'refuse', 'revoke'].includes(command)) throw new CommandError(MOVED)
  // `sessions` is the older name of `chats` and keeps working.
  if (command === 'chats' || command === 'sessions') return chatsArgs(tokens)
  if (!['status', 'join', 'leave', 'ping', 'up', 'down', 'doctor', 'help'].includes(command)) throw new CommandError('Unknown command. Run /switchboard help.')
  if (command === 'join') {
    if (tokens[0] === '--') tokens.shift()
    else if (tokens.some(token => token.startsWith('-'))) throw new CommandError('Join takes an optional name, not options. Use -- before a name starting with a dash.')
    const name = tokens.join(' ')
    if (name && !/[A-Za-z0-9]/.test(name)) throw new CommandError('The name needs at least one letter or digit.')
    return { command, name }
  }
  if (command === 'ping') {
    if (tokens.length !== 1 || tokens[0].startsWith('-')) throw new CommandError('Ping takes one chat, for example /switchboard ping laptop/research.')
    return { command, target: tokens[0] }
  }
  if (tokens.length) throw new CommandError('/switchboard ' + command + ' takes no arguments. Run /switchboard help.')
  return { command }
}

function chatsArgs(tokens) {
  let scope = 'all'
  let details = false
  let literal = false
  const search = []
  for (const token of tokens) {
    if (!literal && token === '--') { literal = true; continue }
    if (!literal && ['--local', '--remote'].includes(token)) {
      if (scope !== 'all') throw new CommandError('Choose either --local or --remote once, not both.')
      scope = token.slice(2)
    } else if (!literal && token === '--details') {
      if (details) throw new CommandError('Use --details only once.')
      details = true
    } else if (!literal && token.startsWith('-')) throw new CommandError('Chats takes --local, --remote, --details, and search words. Use -- before a search starting with a dash.')
    else search.push(token)
  }
  let query = search.join(' ')
  if (query.length > 1 && ((query.startsWith('"') && query.endsWith('"')) || (query.startsWith("'") && query.endsWith("'")))) query = query.slice(1, -1)
  return { command: 'chats', scope, query, details }
}

export function daysLeft(expiresAt, now = Date.now() / 1000) {
  return Math.max(0, Math.ceil((expiresAt - now) / 86400))
}

export function expiry(row, now) {
  if (!row.enrolled || typeof row.expiresAt !== 'number') return ''
  const days = daysLeft(row.expiresAt, now)
  return days <= 0 ? 'expires today' : 'expires in ' + days + (days === 1 ? ' day' : ' days')
}

export function formatStatus(status, id, overview = false, now) {
  const current = status.sessions.find(row => row.id === id)
  const membership = !validSession(id) ? 'Unknown (this chat’s ID isn’t available)' : !current ? 'Unknown (not found on this machine)'
    : current.enrolled ? ['Joined as ' + (current.joinedName || current.name), expiry(current, now)].filter(Boolean).join(', ') : 'Not joined'
  const server = status.server ? ' to ' + status.server : ''
  const headline = !status.enabled ? 'Bridge turned off' : status.connected ? 'Connected' + server : 'Not connected' + server
  const next = !status.enabled || !status.connected || !current ? 'doctor' : current.enrolled ? 'chats' : 'join'
  const joined = status.sessions.filter(row => row.enrolled).length
  const total = status.sessions.length
  return [
    { kind: 'title', text: headline, tone: status.enabled && status.connected ? 'success' : 'warning' },
    { kind: 'fact', label: 'Machine', value: status.machine || 'Not reported' },
    { kind: 'fact', label: 'Chat', value: current ? current.name : 'Unavailable' },
    { kind: 'fact', label: 'Membership', value: membership },
    ...(!overview && validSession(id) ? [{ kind: 'fact', label: 'ID', value: id, quiet: true, mono: true }] : []),
    { kind: 'fact', label: 'Chats here', value: joined + ' of ' + total + ' joined' },
    ...(status.detail ? [{ kind: 'fact', label: 'Detail', value: status.detail, quiet: true }] : []),
    { kind: 'fact', space: true, label: 'Try', value: '/switchboard ' + next, mono: true },
  ]
}

export function formatSessions(roster, id, scope, query, details = false, now) {
  const words = query.toLowerCase().split(/\s+/).filter(Boolean)
  const matches = roster.rows.filter(row => {
    const haystack = [row.name, row.joinedName, row.id, row.machine, row.project, row.repository, row.branch].join(' ').toLowerCase()
    return words.every(word => haystack.includes(word))
  }).sort((a, b) => Number(b.local) - Number(a.local) || a.machine.localeCompare(b.machine)
    || Number(b.local && b.id === id) - Number(a.local && a.id === id)
    || Number(b.enrolled) - Number(a.enrolled) || a.name.localeCompare(b.name) || a.id.localeCompare(b.id))
  const title = scope === 'local' ? 'Chats on this machine' : scope === 'remote' ? 'Chats on other machines' : 'Chats'
  const doc = [{ kind: 'title', text: title + (query ? ' matching “' + clean(query, 256) + '”' : '') }]
  const groups = new Map()
  for (const row of matches.slice(0, 50)) {
    const key = (row.local ? 'local:' : 'remote:') + row.machine
    if (!groups.has(key)) groups.set(key, { local: row.local, heading: row.machine + (row.local ? ' (this machine)' : ''), rows: [] })
    groups.get(key).rows.push(row)
  }
  const current = row => row.local && row.id === id
  for (const group of groups.values()) {
    doc.push({ kind: 'heading', space: true, text: group.heading })
    if (details) {
      for (const row of group.rows) {
        doc.push({ kind: 'text', space: true, text: row.name + (current(row) ? ' (this chat)' : '') })
        const facts = [['Activity', activity(row.status)],
          ...(row.local ? [['Membership', row.enrolled ? 'Joined' : 'Not joined']] : []),
          ...(row.enrolled ? [['Joined as', row.joinedName], ['Expires', expiry(row, now).replace(/^expires /, '')]] : []),
          ['Project', row.project], ['Repository', row.repository], ['Branch', row.branch]]
        for (const [label, value] of facts) if (value) doc.push({ kind: 'fact', label, value, quiet: label !== 'Membership' })
        doc.push({ kind: 'fact', label: 'ID', value: row.id, quiet: true, mono: true })
      }
    } else {
      const columns = group.local ? ['Chat', 'Activity', 'Membership', 'Expires'] : ['Chat', 'Activity']
      doc.push({ kind: 'table', columns, rows: group.rows.map(row => ({
        cells: [row.name, activity(row.status), ...(group.local ? [row.enrolled ? 'Joined as ' + (row.joinedName || row.name) : 'Not joined', expiry(row, now).replace(/^expires /, '')] : [])],
        ...(current(row) ? { note: '(this chat)' } : {}),
      })) })
    }
  }
  if (!matches.length && roster.remoteAvailable !== false) doc.push({ kind: 'text', space: true, text: query ? 'No matching chats.' : 'No chats found.' })
  if (!matches.length && roster.remoteAvailable === false && scope !== 'remote') doc.push({ kind: 'text', space: true, text: query ? 'No matching chats on this machine.' : 'No chats on this machine.' })
  if (matches.length > 50) doc.push({ kind: 'text', space: true, text: 'Showing 50 of ' + matches.length + ' chats. Add search words to narrow the list.' })
  if (roster.remoteAvailable === false) doc.push({ kind: 'text', space: true, tone: 'warning', text: roster.remoteError })
  if (!validSession(id)) doc.push({ kind: 'text', space: true, quiet: true, text: 'This chat’s ID isn’t available, so it isn’t marked.' })
  if (!details && matches.length) {
    const safeQuery = clean(query)
    const hint = safeQuery === query
      ? '/switchboard chats' + (scope !== 'all' ? ' --' + scope : '') + ' --details' + (query ? ' -- ' + query : '')
      : 'add --details to this command'
    doc.push({ kind: 'fact', space: true, label: 'Full IDs and details', value: hint, quiet: true, mono: safeQuery === query })
  }
  return doc
}

// The current CLI reports program, configuration, bridge, discovery (only when
// something is off), server, and chats. The rest are check names from older CLIs.
const CHECK_NAMES = { program: 'Program', configuration: 'Configuration', bridge: 'Bridge', discovery: 'Chat discovery', server: 'Server', chats: 'Joined chats',
  binary: 'Program', python: 'Python', installation: 'Installation', service: 'Service', enabled: 'Bridge turned on', daemon: 'Bridge',
  connection: 'Server connection', sessions: 'Joined chats', tls: 'Machine key', hub_health: 'Server', identity: 'Identity' }
function checkName(name) {
  const label = clean(name, 64).replace(/_+/g, ' ')
  return CHECK_NAMES[name] || label.charAt(0).toUpperCase() + label.slice(1)
}

export function formatDoctor(checks) {
  const failed = checks.filter(check => check.status === 'fail')
  const warnings = checks.filter(check => check.status === 'warn')
  const passed = checks.filter(check => check.status === 'pass')
  const counts = [
    ...(failed.length ? [failed.length + (failed.length === 1 ? ' problem' : ' problems')] : []),
    ...(warnings.length ? [warnings.length + (warnings.length === 1 ? ' warning' : ' warnings')] : []),
  ]
  const summary = counts.length ? counts.join(' and ') + '.' : 'All checks passed.'
  const doc = [{ kind: 'title', text: 'Checks', tone: failed.length ? 'error' : warnings.length ? 'warning' : 'success' },
    { kind: 'text', quiet: true, text: summary }]
  for (const [title, rows, tone] of [['Needs action', failed, 'error'], ['Attention', warnings, 'warning']]) if (rows.length) {
    doc.push({ kind: 'heading', space: true, text: title, tone })
    for (const check of rows) {
      doc.push({ kind: 'item', space: true, title: checkName(check.name), text: check.summary,
        ...(check.fix && check.fix !== check.summary ? { note: 'To fix: ' + continued(check.fix) } : {}) })
    }
  }
  if (passed.length) doc.push({ kind: 'text', space: true, quiet: true, text: 'Passed: ' + passed.map(check => checkName(check.name)).join(', ') + '.' })
  return doc
}

export function formatSelection(selection, action) {
  if (action === 'join') {
    if (!selection.confirmed) return [
      { kind: 'title', tone: 'warning', text: 'Joined as ' + selection.name + (selection.publishedName
        ? ', but other machines still see it as ' + selection.publishedName + '.' : ', but not live yet.') },
      { kind: 'text', text: selection.detail.charAt(0).toUpperCase() + selection.detail.slice(1) + '.' },
      { kind: 'text', quiet: true, text: 'Run /switchboard doctor if this persists.' },
    ]
    const { remoteSessions: sessions, remoteMachines: machines } = selection
    return [
      { kind: 'title', tone: 'success', text: 'Joined as ' + selection.name + '.' },
      { kind: 'text', text: !sessions ? 'No other machine has a joined chat right now.'
        : 'Reachable by ' + sessions + (sessions === 1 ? ' chat' : ' chats') + ' on ' + machines + ' other ' + (machines === 1 ? 'machine.' : 'machines.') },
      { kind: 'text', quiet: true, text: 'Leaves automatically after 7 days without work or messages.' },
    ]
  }
  if (selection.changed === false) return [{ kind: 'title', text: 'This chat wasn’t joined; nothing changed.' }]
  if (!selection.confirmed) return [
    { kind: 'title', tone: 'warning', text: 'This chat left, but the bridge hasn’t confirmed it yet.' },
    { kind: 'text', text: selection.detail.charAt(0).toUpperCase() + selection.detail.slice(1) + '.' },
    { kind: 'text', quiet: true, text: 'Run /switchboard doctor if this persists.' },
  ]
  return [{ kind: 'title', tone: 'success', text: 'This chat left.' }, { kind: 'text', text: 'Other machines can no longer see it.' }]
}

export function formatPing(result, target) {
  if (result.unsupported) return [{ kind: 'text', text: 'This version of Switchboard can’t ping. Update Switchboard with switchboard upgrade in a terminal.' }]
  if (!result.ok) return [
    { kind: 'title', tone: 'warning', text: 'No reply from ' + clean(target, 128) + '.' },
    { kind: 'text', text: sentence(result.error) },
  ]
  return [{ kind: 'title', tone: 'success', text: result.machine + '/' + result.name + ' replied in ' + result.rttMs + ' ms.' }]
}

