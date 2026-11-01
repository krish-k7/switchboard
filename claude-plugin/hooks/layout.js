// One layout source. Formatters build a document of blocks; plainText() is the
// stored command result and draw() is the native tree. Neither parses the other.
//
// Blocks:
//   { kind: 'title', text, tone? }              tone: 'success' | 'warning' | 'error'
//   { kind: 'heading', text, tone? }
//   { kind: 'fact', label, value, quiet?, mono? }
//   { kind: 'text', text, quiet?, tone?, strong? }
//   { kind: 'code', text }
//   { kind: 'item', title?, text, note?, tone? } a check or cause, and its recovery
//   { kind: 'table', columns: [title...], rows: [{ cells: [...], note? }], fit? }
//                                               fit: the first column takes its content's width
// Any block may set space: true for one blank line before it.

const cells = value => [...String(value)].reduce((width, char) => width + (/[ᄀ-ᅟ⺀-꓏가-힣豈-﫿︰-﹏＀-｠￠-￦]/.test(char) ? 2 : 1), 0)
const pad = (value, width) => value + ' '.repeat(Math.max(0, width - cells(value)))

function factRuns(doc) {
  // Consecutive facts share one label column, in text and in the tree.
  const widths = new Map()
  let run = []
  const close = () => { const width = Math.max(0, ...run.map(block => cells(block.label) + 1)); for (const block of run) widths.set(block, width); run = [] }
  for (const block of doc) {
    if (block.kind === 'fact' && !(block.space && run.length)) run.push(block)
    else { close(); if (block.kind === 'fact') run.push(block) }
  }
  close()
  return widths
}

function firstCell(row) {
  return row.cells[0] + (row.note ? ' ' + row.note : '')
}

export function plainText(doc) {
  const widths = factRuns(doc)
  const lines = []
  for (const block of doc) {
    if (block.space && lines.length) lines.push('')
    if (block.kind === 'fact') lines.push(pad(block.label + ':', widths.get(block)) + '  ' + block.value)
    else if (block.kind === 'code') lines.push('  ' + block.text)
    else if (block.kind === 'item') lines.push('  ' + (block.title ? block.title + ': ' : '') + block.text, ...(block.note ? ['  ' + block.note] : []))
    else if (block.kind === 'table') {
      const table = [block.columns, ...block.rows.map(row => [firstCell(row), ...row.cells.slice(1)])]
      const sizes = block.columns.map((_, index) => Math.max(...table.map(line => cells(line[index] ?? ''))))
      for (const line of table) lines.push(('  ' + line.map((value, index) => index === line.length - 1 ? value ?? '' : pad(value ?? '', sizes[index])).join('  ')).trimEnd())
    } else lines.push(block.text)
  }
  return lines.join('\n')
}

export function draw({ Box, Text }, doc, columns = 80) {
  const width = Number.isFinite(columns) ? Math.max(1, Math.floor(columns)) : 80
  const gutter = width < 40 ? 0 : 2
  const contentWidth = Math.min(88, width - gutter)
  const widths = factRuns(doc)
  const text = (value, style = {}) => Text({ wrap: 'wrap', ...style, children: [value] })
  const spaced = (block, node) => block.space ? Box({ marginTop: 1, flexDirection: 'column', children: [node] }) : node
  const fact = (label, value, labelWidth, style = {}) => Box({ flexDirection: 'row', columnGap: 2, children: [
    Box({ width: Math.min(14, Math.max(1, labelWidth)), flexShrink: 0, children: [text(label, { dimColor: true })] }),
    Box({ flexGrow: 1, flexShrink: 1, minWidth: 0, children: [text(value, style)] }),
  ] })
  const nodes = []
  for (const block of doc) {
    if (block.kind === 'title' || block.kind === 'heading') nodes.push(spaced(block, text(block.text, { weight: 'semibold', ...(block.tone ? { color: block.tone } : {}) })))
    else if (block.kind === 'fact') nodes.push(spaced(block, fact(block.label, block.value, widths.get(block) - 1, { ...(block.quiet ? { dimColor: true } : {}), ...(block.mono ? { mono: true } : {}) })))
    else if (block.kind === 'code') nodes.push(spaced(block, text(block.text, { mono: true, dimColor: true })))
    else if (block.kind === 'item') nodes.push(spaced(block, Box({ flexDirection: 'column', children: [
      ...(block.title ? [text(block.title, { weight: 'semibold' }), text(block.text)]
        : [text(block.text, { ...(block.tone === 'error' ? { weight: 'semibold' } : {}), ...(block.tone ? { color: block.tone } : {}) })]),
      ...(block.note ? [text(block.note, { dimColor: true })] : []),
    ] })))
    else if (block.kind === 'text') nodes.push(spaced(block, text(block.text, { ...(block.quiet ? { dimColor: true } : {}), ...(block.tone ? { color: block.tone } : {}), ...(block.strong ? { weight: 'semibold' } : {}) })))
    else if (block.kind === 'table') {
      const rest = block.columns.slice(1)
      const name = row => Box({ flexDirection: 'row', columnGap: 1, children: [
        Box({ flexShrink: 1, minWidth: 0, children: [text(row.cells[0], { weight: 'semibold', wrap: 'truncate-end' })] }),
        ...(row.note ? [Box({ flexShrink: 0, children: [text(row.note, { dimColor: true })] })] : []),
      ] })
      if (contentWidth >= 60) {
        // Fixed related columns stay together near the left edge; the last one
        // takes the remaining width and wraps rather than cutting prose short.
        const sizes = rest.map((title, index) => Math.min(24, Math.max(cells(title), ...block.rows.map(row => cells(row.cells[index + 1] ?? '')))))
        const firstWidth = block.fit ? Math.min(32, Math.max(cells(block.columns[0]), ...block.rows.map(row => cells(firstCell(row))))) : 32
        const line = (first, values, heading) => Box({ flexDirection: 'row', columnGap: 2, children: [
          Box({ width: firstWidth, flexShrink: 1, minWidth: 0, children: [first] }),
          ...values.map((value, index) => index === values.length - 1
            ? Box({ flexGrow: 1, flexShrink: 1, minWidth: sizes[index], children: [text(value ?? '', { dimColor: heading || index === 0 })] })
            : Box({ width: sizes[index], flexShrink: 0, children: [text(value ?? '', { dimColor: heading || index === 0, wrap: 'truncate-end' })] })),
        ] })
        nodes.push(spaced(block, Box({ flexDirection: 'column', children: [
          line(text(block.columns[0], { dimColor: true }), rest, true),
          ...block.rows.map(row => line(name(row), row.cells.slice(1), false)),
        ] })))
      } else {
        const labelWidth = Math.max(...rest.map(title => cells(title)))
        nodes.push(spaced(block, Box({ flexDirection: 'column', children: block.rows.map((row, index) => Box({ flexDirection: 'column', marginTop: index ? 1 : 0, children: [
          name(row),
          ...rest.map((title, column) => row.cells[column + 1] ? fact(title, row.cells[column + 1], labelWidth, { dimColor: true }) : null).filter(Boolean),
        ] })) })))
      }
    }
  }
  return Box({ flexDirection: 'column', paddingLeft: gutter, children: [
    Box({ flexDirection: 'column', width: contentWidth, minWidth: 0, children: nodes }),
  ] })
}
