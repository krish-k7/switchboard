import { draw } from './layout.js'

// Layout documents of this session's results, keyed by their exact stored text,
// so each transcript row draws from its own document. Rows the map does not
// know (from before a reload, or host errors) are drawn as literal text.
const documents = new Map()
const LIMIT = 200

export function remember(text, doc) {
  documents.delete(text)
  documents.set(text, doc)
  if (documents.size > LIMIT) documents.delete(documents.keys().next().value)
}

export function commandOutput(ui, props, columns) {
  const doc = props.isErrored ? null : documents.get(props.text) ?? documents.get(props.text.replace(/^switchboard: ?/, ''))
  return draw(ui, doc ?? props.text.split('\n').map(line => ({ kind: 'text', text: line, ...(props.isErrored ? { tone: 'error' } : {}) })), columns)
}
