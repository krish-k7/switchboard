// Project only documented CLI fields; never return raw stdout or stderr.
// A failure the plugin explains itself. `code` is an optional command line to
// show on its own line after the message (the plugin's own text, never CLI output).
export class CommandError extends Error {
  constructor(message, code = '') {
    super(message)
    this.code = code
  }
}

export const NOT_SET_UP = 'This machine isn’t set up yet. To fix, run switchboard init in a terminal.'
// The CLI's error when this machine has no configuration. Older CLIs said
// "Switchboard is not set up on this machine (no /path/to/bridge.json)".
export function notSetUp(reason) {
  return typeof reason === 'string' && /^switchboard (?:isn't|isn’t|is not) set up on this machine\b/i.test(reason.trim())
}

// Text from the CLI, the server, or another machine (chat and machine names,
// details, errors) as this plugin shows it. What a command prints reaches the
// model too, so nothing in it may read as markup: < and > become the
// single angle quotes ‹ and ›, and a name like <system-reminder> stays
// visible but can't open or close a tag. Control characters become spaces.
export function clean(value, limit = 512) {
  return typeof value === 'string'
    ? value.replace(/https?:\/\/\S+/gi, '[URL omitted]')
      .replace(/bearer\s+\S+/gi, '[credential omitted]')
      .replace(/[\u0000-\u001f\u007f-\u009f\u202a-\u202e\u2066-\u2069]/g, ' ')
      .replace(/[<\uff1c\u2329\u3008]/g, '‹').replace(/[>\uff1e\u232a\u3009]/g, '›')
      .slice(0, limit).trim()
    : ''
}

export function validSession(value) {
  return typeof value === 'string' && /^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}$/i.test(value)
}

function object(value, kind) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) throw new CommandError('Switchboard returned an invalid ' + kind + ' response.')
}

function sessionRows(value, local, machine) {
  if (!Array.isArray(value)) throw new CommandError('Switchboard returned an invalid chat list.')
  const keys = new Set()
  return value.map(row => {
    object(row, 'chat')
    if (!validSession(row.id) || typeof row.name !== 'string' || typeof row.status !== 'string' || local && typeof row.enrolled !== 'boolean') throw new CommandError('Switchboard returned an invalid chat row.')
    const label = local ? machine || 'This machine' : clean(row.machine, 64)
    if (!label) throw new CommandError('Switchboard returned a chat without a machine.')
    const key = label + '/' + row.id
    if (keys.has(key)) throw new CommandError('Switchboard returned the same chat twice.')
    keys.add(key)
    for (const field of ['project', 'repository', 'branch', 'joined_name']) if (row[field] !== undefined && typeof row[field] !== 'string') throw new CommandError('Switchboard returned invalid chat details.')
    if (row.expires_at !== undefined && row.expires_at !== null && !Number.isFinite(row.expires_at)) throw new CommandError('Switchboard returned invalid chat details.')
    return { id: row.id, name: clean(row.name, 256) || '(unnamed chat)', status: clean(row.status, 64),
      local, machine: label, enrolled: local && row.enrolled,
      joinedName: local && row.enrolled ? clean(row.joined_name, 64) : '', expiresAt: local && row.enrolled && Number.isFinite(row.expires_at) ? row.expires_at : null,
      project: clean(row.project, 256), repository: clean(row.repository), branch: clean(row.branch, 256) }
  })
}

export function parseStatus(value) {
  object(value, 'status')
  if (typeof value.enabled !== 'boolean' || typeof value.connected !== 'boolean' || value.connected && !value.enabled) throw new CommandError('Switchboard returned an invalid connection state.')
  return { enabled: value.enabled, connected: value.connected, machine: clean(value.machine, 64), detail: clean(value.detail),
    server: serverHost(value.server_url), sessions: sessionRows(value.sessions, true, clean(value.machine, 64)) }
}

// The server's host (and port) from its HTTPS address, for display only. clean()
// would replace the whole URL, so take the authority apart here and keep it only
// if it is a plain host name or address of sensible length.
export function serverHost(url) {
  if (typeof url !== 'string') return ''
  const match = /^https?:\/\/(?:[^@\/?#\s]*@)?([^\/?#\s]+)/i.exec(url.trim())
  const host = match ? match[1].replace(/\.$/, '') : ''
  return host.length <= 64 && /^(?:[A-Za-z0-9.-]+|\[[0-9A-Fa-f:.]+\])(?::[0-9]{1,5})?$/.test(host) ? host : ''
}

export function parseRoster(value, scope = 'all') {
  object(value, 'chat list')
  if (typeof value.enabled !== 'boolean') throw new CommandError('Switchboard returned an invalid chat list.')
  const machine = clean(value.machine, 64)
  const localRows = sessionRows(value.sessions, true, machine)
  if (scope === 'local') return { rows: localRows, remoteAvailable: null, remoteError: '' }
  if (typeof value.remote_available !== 'boolean' || !Array.isArray(value.remote_sessions)) throw new CommandError('Switchboard returned an invalid list of chats on other machines.')
  const remoteRows = value.remote_available ? sessionRows(value.remote_sessions, false, machine) : []
  return { rows: scope === 'remote' ? remoteRows : [...localRows, ...remoteRows],
    remoteAvailable: value.remote_available, remoteError: clean(value.remote_error) || 'Chats on other machines are unavailable.' }
}

export function parseDoctor(value) {
  object(value, 'diagnostic')
  if (typeof value.ok !== 'boolean' || !Array.isArray(value.checks) || !value.checks.length || value.checks.length > 128) throw new CommandError('Switchboard returned an invalid diagnostic report.')
  const names = new Set()
  const checks = value.checks.map(check => {
    object(check, 'diagnostic check')
    if (typeof check.name !== 'string' || !clean(check.name) || names.has(check.name) || !['pass', 'warn', 'fail'].includes(check.status) || typeof check.summary !== 'string' || !clean(check.summary) || check.fix !== undefined && typeof check.fix !== 'string') throw new CommandError('Switchboard returned an invalid diagnostic check.')
    names.add(check.name)
    return { name: clean(check.name, 64), status: check.status, summary: clean(check.summary), fix: clean(check.fix) }
  })
  if (value.ok !== !checks.some(check => check.status === 'fail')) throw new CommandError('Switchboard returned contradictory diagnostic results.')
  return checks
}

export function parseSelection(value, action, id) {
  object(value, 'selection')
  if (value.ok !== true || value.action !== action || value.session !== id || value.name !== undefined && typeof value.name !== 'string') throw new CommandError('Switchboard did not confirm this chat’s ' + action + ' request.')
  const name = clean(value.name, 256) || 'this chat'
  // Leaving a chat that was not joined changes nothing, so there is nothing to confirm.
  if (action === 'leave' && value.changed === false) return { id, name, changed: false, confirmed: true, detail: '', remoteSessions: 0, remoteMachines: 0 }
  if (typeof value.confirmed !== 'boolean') throw new CommandError('The ' + action + ' was saved, but this version of Switchboard can’t confirm it reached the bridge. Update Switchboard with switchboard upgrade in a terminal.')
  const count = key => Number.isInteger(value[key]) && value[key] >= 0 ? value[key] : 0
  if (!value.confirmed && typeof value.detail !== 'string') throw new CommandError('Switchboard returned an invalid ' + action + ' result.')
  // A rename the bridge hasn't published yet: other machines still see this name.
  const publishedName = typeof value.published_name === 'string' ? clean(value.published_name, 256) : ''
  return { id, name, changed: true, confirmed: value.confirmed, detail: clean(value.detail) || 'the bridge has not confirmed this change',
    remoteSessions: count('remote_sessions'), remoteMachines: count('remote_machines'), publishedName }
}

// An installed CLI too old for a command: its parser refuses the command name
// ("unknown command"; older CLIs said "invalid choice").
const UNKNOWN_COMMAND = /unknown command|invalid choice/i

// Ping: success carries the reached chat and round trip; anything else is a plain failure.
export function parsePing(result) {
  let value = null
  for (const stream of [result.stdout, result.stderr]) {
    try { const parsed = JSON.parse(stream); if (parsed && typeof parsed === 'object' && !Array.isArray(parsed)) { value = parsed; break } } catch { /* try the other stream */ }
  }
  if (result.exitCode === 2 && UNKNOWN_COMMAND.test(value?.error || result.stderr || '')) return { unsupported: true }
  if (result.exitCode === 0 && value?.ok === true && typeof value.machine === 'string' && typeof value.name === 'string'
      && Number.isFinite(value.rtt_ms) && value.rtt_ms >= 0 && typeof value.via === 'string') {
    return { ok: true, machine: clean(value.machine, 64), name: clean(value.name, 64), rttMs: Math.round(value.rtt_ms), via: clean(value.via, 32) || 'server' }
  }
  if (value?.ok === false && notSetUp(value.error)) throw new CommandError(NOT_SET_UP)
  if (value?.ok === false && typeof value.error === 'string' && clean(value.error)) return { ok: false, error: clean(value.error) }
  throw new CommandError('Switchboard returned an invalid ping result.')
}

export function activity(value) {
  return ({ idle: 'Idle', busy: 'Busy', waiting: 'Waiting', shell: 'Shell' })[value] || ''
}
