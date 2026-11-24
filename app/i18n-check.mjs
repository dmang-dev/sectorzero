// Checks the UI translations in ui/locales/ against the English text in ui/index.html and
// ui/app.js (the keys): every catalog must translate every key, keep each {placeholder}, and
// keep the markup of data-i18n-html strings; keys no longer used are reported too.
//
//   node app/i18n-check.mjs            check every catalog (exit 1 on problems)
//   node app/i18n-check.mjs --keys     print the keys as JSON (for translators)
//   node app/i18n-check.mjs --missing de   print the keys de lacks, as JSON
import { readFileSync, readdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

const UI = join(dirname(fileURLToPath(import.meta.url)), 'ui');
const norm = (s) => s.replace(/\s+/g, ' ').trim();
const decode = (s) => s.replace(/&quot;/g, '"').replace(/&#39;/g, "'").replace(/&lt;/g, '<')
  .replace(/&gt;/g, '>').replace(/&nbsp;/g, ' ').replace(/&amp;/g, '&');

// Same rules as translatePage() in i18n.js: text nodes and title / placeholder / aria-label,
// outside script, style, title and data-no-i18n; data-i18n-html elements as one HTML string.
function htmlKeys(html) {
  const keys = new Set();
  const skip = [];   // open elements whose text is skipped
  let capture = null; // { tag, depth, start } for a data-i18n-html element
  const re = /<!--[\s\S]*?-->|<![^>]*>|<(\/?)([a-zA-Z][\w-]*)([^>]*?)(\/?)>|([^<]+)/g;
  const voids = new Set(['meta', 'link', 'input', 'br', 'img', 'hr']);
  for (let m = re.exec(html); m; m = re.exec(html)) {
    const [whole, close, tagRaw, attrs, , text] = m;
    if (text !== undefined) {
      if (!capture && !skip.length && norm(decode(text))) keys.add(norm(decode(text)));
      continue;
    }
    if (!tagRaw) continue;
    const tag = tagRaw.toLowerCase();
    if (capture) {
      if (tag === capture.tag) capture.depth += close ? -1 : 1;
      if (capture.depth === 0) {
        keys.add(norm(html.slice(capture.start, m.index)));
        capture = null;
      }
      continue;
    }
    if (close) {
      if (skip.length && skip[skip.length - 1] === tag) skip.pop();
      continue;
    }
    if (!skip.length && !/data-no-i18n/.test(attrs)) {
      for (const a of attrs.matchAll(/\b(title|placeholder|aria-label)="([^"]*)"/g)) {
        if (norm(decode(a[2]))) keys.add(norm(decode(a[2])));
      }
    }
    if (/\bdata-i18n-html\b/.test(attrs)) {
      capture = { tag, depth: 1, start: m.index + whole.length };
    } else if (!voids.has(tag) && (['script', 'style', 'title'].includes(tag) || /data-no-i18n/.test(attrs) || skip.length)) {
      skip.push(tag);
    }
  }
  return keys;
}

// t('…') calls with a literal first argument.
function jsKeys(js) {
  const keys = new Set();
  for (const m of js.matchAll(/\bt\(\s*'((?:[^'\\]|\\.)*)'/g)) {
    keys.add(m[1].replace(/\\(.)/g, (_, c) => ({ n: '\n', t: '\t' }[c] ?? c)));
  }
  return keys;
}

const placeholders = (s) => [...s.matchAll(/\{(\w+)\}/g)].map((m) => m[1]).sort().join(',');
const tags = (s) => [...s.matchAll(/<\/?([a-z]+)/gi)].map((m) => m[0].toLowerCase()).sort().join(',');

function loadCatalog(file) {
  const window = { SZ_LOCALES: {} };
  vm.runInNewContext(readFileSync(join(UI, 'locales', file), 'utf8'), { window }, { filename: file });
  return Object.entries(window.SZ_LOCALES);
}

const keys = new Set([
  ...htmlKeys(readFileSync(join(UI, 'index.html'), 'utf8')),
  ...jsKeys(readFileSync(join(UI, 'app.js'), 'utf8')),
]);
// Not translated: the product name, symbols and technical tokens that read the same everywhere.
for (const k of [...keys]) if (k === 'SectorZero' || /^[\d\s.,:;×x/()+\-–—›▾…]*$/.test(k) || /^(0x[0-9A-F]+|[0-9A-F]{4}-[0-9A-F]{4}|FAT\d+|MBR|GPT|OK)$/.test(k)) keys.delete(k);

const arg = process.argv[2];
if (arg === '--keys') {
  console.log(JSON.stringify([...keys], null, 1));
  process.exit(0);
}

const i18nSrc = readFileSync(join(UI, 'i18n.js'), 'utf8');
const listed = [...i18nSrc.matchAll(/\['([\w-]+)', '[^']+'\]/g)].map((m) => m[1]).filter((c) => c !== 'en');
let problems = 0;
const report = (msg) => { problems++; console.log(msg); };
const files = new Set(readdirSync(join(UI, 'locales')).filter((f) => f.endsWith('.js')));

if (arg === '--missing') {
  const want = process.argv[3];
  const dict = files.has(`${want}.js`) ? Object.fromEntries(loadCatalog(`${want}.js`))[want] ?? {} : {};
  console.log(JSON.stringify([...keys].filter((k) => !Object.hasOwn(dict, k)), null, 1));
  process.exit(0);
}

for (const code of listed) {
  if (!files.has(`${code}.js`)) { report(`${code}: listed in i18n.js but locales/${code}.js is missing`); continue; }
  const entries = loadCatalog(`${code}.js`);
  if (entries.length !== 1 || entries[0][0] !== code) { report(`${code}.js must set window.SZ_LOCALES['${code}'] only`); continue; }
  const dict = entries[0][1];
  const missing = [...keys].filter((k) => !Object.hasOwn(dict, k));
  for (const k of missing) report(`${code}: missing ${JSON.stringify(k)}`);
  for (const [k, v] of Object.entries(dict)) {
    if (!keys.has(k)) { report(`${code}: unused ${JSON.stringify(k)}`); continue; }
    if (typeof v !== 'string' || !v.trim()) report(`${code}: empty ${JSON.stringify(k)}`);
    else if (placeholders(k) !== placeholders(v)) report(`${code}: placeholders differ in ${JSON.stringify(k)} → ${JSON.stringify(v)}`);
    else if (tags(k) !== tags(v)) report(`${code}: markup differs in ${JSON.stringify(k)} → ${JSON.stringify(v)}`);
  }
}
for (const f of files) if (!listed.includes(f.replace(/\.js$/, ''))) report(`locales/${f} is not listed in i18n.js`);

console.log(`${keys.size} strings, ${listed.length} languages, ${problems} problem(s)`);
process.exit(problems ? 1 : 0);
