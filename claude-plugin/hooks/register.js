import { CommandError, NOT_SET_UP, clean, notSetUp, validSession, parseStatus, parseRoster, parseDoctor, parseSelection, parsePing } from './data.js'
import { HELP, errorDoc, parseArgs, sentence, formatStatus, formatSessions, formatDoctor, formatSelection, formatPing } from './format.js'
import { fromDisplay, NOT_SET_UP_DISPLAY } from './display.js'
import { plainText } from './layout.js'
import { commandOutput, remember } from './output.js'

let epoch = 0
// The queue stores completion only. Every request owns its identity and reply.
let tail = Promise.resolve()
// This chat's footer label. The bridge and CLI keep membership.json current;
// Claude Code watches it for us (watchPaths), so no chat runs a process of its own.
// Each prompt and each join, leave, or status also refreshes it.
let footer = { session: null, joined: false, name: '' }
// Bumped by every label a command sets: a file read started earlier must not undo it.
let labelVersion = 0
let refreshes = 0
const MEMBERSHIP_FILE = 'membership.json'
const INSTALL = 'curl -fsSL https://raw.githubusercontent.com/krish-k7/switchboard/main/install.sh | sh'
// What a person calls each CLI command: the CLI's `sessions` is `chats` here.
const spoken = action => action === 'sessions' ? 'chats' : action

function setFooter($, session, joined, name) {
  if (footer.session === session && footer.joined === joined && footer.name === name) return
  footer = { session, joined, name }
  try { $.ui.invalidate('ui.render') } catch { /* The label refreshes on the next draw. */ }
}

function commandFooter($, session, joined, name) {
  labelVersion += 1
  setFooter($, session, joined, name)
}

// The CLI's own rule (pathlib normalization, no ~ expansion): an absolute path
// without empty and "." parts. Relative paths depend on the CLI's directory.
function normalized(path) {
  if (typeof path !== 'string' || !path.startsWith('/')) return null
  return '/' + path.split('/').filter(part => part && part !== '.').join('/')
}

function active(request) {
  if (request.epoch !== epoch) throw new CommandError('This chat was cleared or switched; run the command again.' + (request.mutationStarted ? ' The earlier ' + request.action + ' may still finish.' : ''))
  if (request.signal?.aborted) throw new CommandError(request.mutationStarted ? 'Request canceled after the ' + request.action + ' started; it may still finish.' : 'Request canceled before it finished.')
}

async function executable($, request) {
  const check = () => { if (request) active(request) }
  check()
  let home
  try { home = await $.env.get('HOME') } catch { throw new CommandError('Your home directory isn’t available, so Switchboard can’t be found. Check HOME in a terminal.') }
  check()
  if (typeof home !== 'string' || !home.startsWith('/')) throw new CommandError('Your home directory isn’t available, so Switchboard can’t be found. Check HOME in a terminal.')
  const path = home.replace(/\/$/, '') + '/bin/switchboard'
  let exists
  try { exists = await $.fs.exists(path) } catch { throw new CommandError('Switchboard can’t check ~/bin/switchboard. Check its permissions in a terminal.') }
  check()
  if (!exists) throw new CommandError('Switchboard isn’t installed on this machine (~/bin/switchboard is missing). To install it, run this in a terminal:', INSTALL)
  return path
}

// A CLI from before --display (0.2 and earlier) refuses the newer options;
// the plugin then asks again the old way and formats the answer itself.
class OlderCli extends Error {}
const OLDER_OPTIONS = /^unknown option --(?:display|details)\b|^--chat only works with join and leave\b|takes no argument \(unexpected/
const UNKNOWN_COMMAND = /unknown command|invalid choice/i

async function cli($, action, extra, request) {
  const path = await executable($, request)
  let result
  try {
    request.mutationStarted = ['join', 'leave', 'up', 'down'].includes(action)
    // Join and leave wait up to ten seconds for the bridge to confirm; up waits
    // for the service manager and then for the connection.
    const timeoutMs = action === 'doctor' ? 30000 : ['up', 'down'].includes(action) ? 60000 : request.mutationStarted ? 20000 : 12000
    result = await $.process.run([path, action, '--json', ...extra], { timeoutMs })
  } catch {
    active(request)
    throw new CommandError('The ' + spoken(action) + ' command didn’t finish. Run switchboard doctor in a terminal to see why.')
  }
  active(request)
  if (!result || !Number.isInteger(result.exitCode) || typeof result.stdout !== 'string') throw new CommandError('Switchboard returned an invalid process response.')
  if (action === 'ping' && !extra.includes('--display')) return result
  let value
  try { value = JSON.parse(result.stdout) } catch { throw new CommandError('Switchboard returned invalid JSON for ' + spoken(action) + '. Run switchboard doctor in a terminal to see why.') }
  if (!value || typeof value !== 'object' || Array.isArray(value)) throw new CommandError('Switchboard returned invalid structured output for ' + spoken(action) + '.')
  if (result.exitCode === 2 && value.ok === false && typeof value.error === 'string') {
    if (extra.includes('--display') && OLDER_OPTIONS.test(value.error)) throw new OlderCli()
    if (['up', 'down'].includes(action) && UNKNOWN_COMMAND.test(value.error)) throw new CommandError('This version of Switchboard can’t take this machine up or down. Update Switchboard with switchboard upgrade in a terminal.')
  }
  if (action === 'ping') return value
  const doctorFailure = action === 'doctor' && result.exitCode === 1 && value.ok === false && Array.isArray(value.checks)
  const remoteFailure = action === 'sessions' && extra.includes('--remote') && result.exitCode === 1 && value.remote_available === false
  if (result.exitCode !== 0 && !doctorFailure && !remoteFailure || value.ok === false && action !== 'doctor') {
    // With --json the CLI reports its error as {ok: false, error} on stdout.
    const reason = value.ok === false ? clean(value.error) : ''
    if (notSetUp(reason)) throw new CommandError(NOT_SET_UP)
    throw new CommandError(reason ? sentence(reason) : 'The ' + spoken(action) + ' command failed. Run /switchboard doctor to see why.')
  }
  return value
}

// The command's own display, as the CLI drew it.
function shown(value) {
  if (!Array.isArray(value.display)) throw new CommandError('Switchboard returned no display. Update Switchboard with switchboard upgrade in a terminal.')
  return fromDisplay(value.display)
}

const NO_ID = 'This chat’s ID isn’t available, so it isn’t marked.'

async function execute($, input, request) {
  active(request)
  let id = null
  if (!['doctor', 'ping', 'up', 'down'].includes(input.command)) {
    try {
      const candidate = await $.session.id()
      if (validSession(candidate)) id = candidate
    } catch { /* Read-only commands can still report unknown current identity. */ }
    active(request)
  }
  if (['join', 'leave'].includes(input.command) && !id) throw new CommandError('This chat’s ID isn’t available, so it can’t ' + input.command + '. Try again in a moment.')
  try {
    return await current($, input, request, id)
  } catch (failure) {
    if (!(failure instanceof OlderCli)) throw failure
    return await older($, input, request, id)
  }
}

// With a CLI that draws its own display (0.3 and later).
async function current($, input, request, id) {
  const chat = id ? ['--chat', id] : []
  if (input.command === 'ping') {
    const value = await cli($, 'ping', ['--display', '--', input.target], request)
    if (value.ok === false && notSetUp(value.error)) throw new CommandError(NOT_SET_UP)
    return shown(value)
  }
  if (input.command === 'status') {
    let value
    try {
      value = await cli($, 'status', ['--display', ...chat], request)
    } catch (failure) {
      if (failure instanceof CommandError && failure.message === NOT_SET_UP) return fromDisplay(NOT_SET_UP_DISPLAY)
      throw failure
    }
    const doc = shown(value)
    const status = parseStatus(value)
    const row = status.sessions.find(row => row.id === id)
    if (row) commandFooter($, id, row.enrolled, row.enrolled ? row.joinedName || row.name : '')
    return id ? doc : [...doc, { kind: 'text', space: true, quiet: true, text: NO_ID.replace('marked', 'shown') }]
  }
  if (input.command === 'chats') {
    const extra = [...(input.scope !== 'all' ? ['--' + input.scope] : []), ...(input.details ? ['--details'] : []), '--display', ...chat,
      ...(input.query ? ['--', input.query] : [])]
    const doc = shown(await cli($, 'sessions', extra, request))
    return id ? doc : [...doc, { kind: 'text', space: true, quiet: true, text: NO_ID }]
  }
  if (input.command === 'doctor' || input.command === 'up' || input.command === 'down') return shown(await cli($, input.command, ['--display'], request))
  const extra = ['--session', id, '--display', ...(input.command === 'join' && input.name ? ['--', input.name] : [])]
  const value = await cli($, input.command, extra, request)
  const selection = parseSelection(value, input.command, id)
  commandFooter($, id, input.command === 'join', input.command === 'join' ? selection.name : '')
  return shown(value)
}

// With a CLI from before --display: the plugin formats the answer itself.
async function older($, input, request, id) {
  if (['up', 'down'].includes(input.command)) throw new CommandError('This version of Switchboard can’t take this machine up or down. Update Switchboard with switchboard upgrade in a terminal.')
  if (input.command === 'ping') return formatPing(parsePing(await cli($, 'ping', ['--', input.target], request)), input.target)
  const extra = input.command === 'join' ? ['--session', id, ...(input.name ? ['--', input.name] : [])]
    : input.command === 'leave' ? ['--session', id]
      : input.command === 'chats' && input.scope !== 'all' ? ['--' + input.scope] : []
  // The CLI's command for listing chats is `sessions`.
  const value = await cli($, input.command === 'chats' ? 'sessions' : input.command, extra, request)
  if (input.command === 'status') {
    const status = parseStatus(value)
    const row = status.sessions.find(row => row.id === id)
    if (row) commandFooter($, id, row.enrolled, row.enrolled ? row.joinedName || row.name : '')
    return formatStatus(status, id, input.overview)
  }
  if (input.command === 'chats') return formatSessions(parseRoster(value, input.scope), id, input.scope, input.query, input.details)
  if (input.command === 'doctor') return formatDoctor(parseDoctor(value))
  const selection = parseSelection(value, input.command, id)
  commandFooter($, id, input.command === 'join', input.command === 'join' ? selection.name : '')
  return formatSelection(selection, input.command)
}

async function serialized($, input, signal) {
  const request = { epoch, signal, action: input.command, mutationStarted: false }
  const previous = tail
  let release
  // Reserve the next position before awaiting: concurrent waiters cannot race.
  tail = new Promise(resolve => { release = resolve })
  try {
    await previous
    active(request)
    return await execute($, input, request)
  } finally {
    release()
  }
}

// membership.json sits beside the bridge config the CLI uses: SWITCHBOARD_CONFIG
// when set, else ~/.config/switchboard/bridge.json. Null when that location
// cannot be known here (a relative SWITCHBOARD_CONFIG).
async function membershipPath($) {
  let config = null
  try { config = await $.env.get('SWITCHBOARD_CONFIG') } catch { config = null }
  if (typeof config !== 'string') {
    const home = await $.env.get('HOME')
    if (typeof home !== 'string' || !home.startsWith('/')) throw new CommandError('Home directory unavailable.')
    config = home.replace(/\/+$/, '') + '/.config/switchboard/bridge.json'
  }
  const path = normalized(config)
  if (!path || path === '/') return null
  const parent = path.slice(0, path.lastIndexOf('/'))
  return parent + '/' + MEMBERSHIP_FILE
}

// Read this chat's membership from the small file. A file that cannot be read
// or parsed clears the label rather than keeping one that may be stale; only
// the latest read applies, and never over a label a command set meanwhile.
async function refreshFooter($) {
  const generation = epoch
  const version = labelVersion
  const sequence = ++refreshes
  let id = null
  let entry = null
  try {
    id = await $.session.id()
    if (!validSession(id)) id = null
    const path = id && await membershipPath($)
    if (path) {
      const value = JSON.parse(await $.fs.read(path))
      if (!value || typeof value.sessions !== 'object' || value.sessions === null || Array.isArray(value.sessions)) throw new Error('invalid membership')
      entry = value.sessions[id] || null
    } else if (id) {
      return // Location unknown here: the label follows this chat's commands.
    }
  } catch { entry = null }
  if (generation !== epoch || version !== labelVersion || sequence !== refreshes) return
  const name = entry && typeof entry.name === 'string' ? clean(entry.name, 64) : ''
  setFooter($, id, Boolean(entry), entry ? name || 'Claude' : '')
}

export function register(on) {
  on('session.start', async ($, e, next) => {
    await $.command.register({ name: 'switchboard', immediate: true, description: 'Join, leave, and list chats across your machines', argumentHint: '[status|chats|join [name]|leave|ping <chat>|up|down|doctor|help]' })
    const started = await next(e)
    void refreshFooter($)
    return started
  })

  // Ask Claude Code to watch the membership file for this session.
  on('classic.SessionStart', async ($, e, next) => {
    const result = await next(e)
    let path
    try { path = await membershipPath($) } catch { return result }
    void refreshFooter($)
    if (!path) return result
    return { ...result, watchPaths: [...(result?.watchPaths ?? []), path] }
  })

  on('classic.FileChanged', async ($, e, next) => {
    // The refresh reads the configured file itself, so any reported spelling of it will do.
    if (typeof e.file_path === 'string' && e.file_path.endsWith('/' + MEMBERSHIP_FILE)) void refreshFooter($)
    return next(e)
  })

  // A fallback if a watch misses an atomic replace: at the latest, the next prompt.
  on('classic.UserPromptSubmit', async ($, e, next) => {
    void refreshFooter($)
    return next(e)
  })

  on('session.end', async ($, e, next) => {
    epoch += 1
    setFooter($, null, false, '')
    return next(e)
  })

  on('command.run', { command: 'switchboard' }, async ($, e, next) => {
    let doc
    try {
      const input = parseArgs(e.args)
      if (input.command === 'help') doc = HELP
      else if (input.command === 'removed') doc = [{ kind: 'text', text: 'Switchboard now uses slash commands only. There is no menu or indicator to close. Run /switchboard for status or /switchboard help for commands.' }]
      else doc = await serialized($, input, next.signal)
    } catch (failure) {
      doc = failure instanceof CommandError ? errorDoc(failure.message, failure.code)
        : errorDoc('Switchboard couldn’t finish this request. Run /switchboard doctor to see why.')
    }
    const text = plainText(doc)
    remember(text, doc)
    return { text }
  })

  on('ui.render', { component: 'CommandOutput' }, async ($, e, next) => {
    if (e.props.command !== 'switchboard') return next(e)
    return commandOutput($.ui.resolve(e), e.props, e.viewport?.columns)
  })

  // Joined chats show a quiet footer label; unjoined chats show nothing.
  on('ui.render', { component: 'SessionMode' }, async ($, e, next) => {
    if (!footer.joined) return next(e)
    return next({ ...e, props: { ...e.props, modes: [...e.props.modes, 'switchboard: ' + (footer.name || 'joined')] } })
  })
}
