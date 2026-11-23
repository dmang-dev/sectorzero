// SectorZero UI. Talks to the Rust side through Tauri commands (see src-tauri/src/main.rs).
'use strict';

const { invoke } = window.__TAURI__.core;
const dialog = window.__TAURI__.dialog;
const { getCurrentWebview } = window.__TAURI__.webview;

const IMAGE_EXTS = ['ima', 'img', 'vfd', 'flp', 'dsk'];
const IMAGE_FILTERS = [
  { name: 'Disk images', extensions: IMAGE_EXTS },
  { name: 'All files', extensions: ['*'] },
];

const state = {
  image: null,       // { path, name, info }
  dir: '',           // current folder inside the image, '' = root
  entries: [],
  selected: new Set(), // entry paths
  anchor: null,      // index for shift-click ranges
  busy: false,
};

const $ = (id) => document.getElementById(id);

// ---------- helpers ----------

const fmtBytes = (n) => n.toLocaleString();
const human = (n) => {
  const u = ['bytes', 'KB', 'MB', 'GB', 'TB'];
  let i = 0;
  while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
  return i === 0 ? `${n} bytes` : `${n.toFixed(n < 10 ? 2 : 1)} ${u[i]}`;
};
const join = (dir, name) => (dir ? `${dir}/${name}` : name);
const parent = (dir) => dir.split('/').slice(0, -1).join('/');
const baseName = (p) => p.split(/[\\/]/).pop();
const isImageName = (p) => IMAGE_EXTS.includes(baseName(p).split('.').pop().toLowerCase());

function typeOf(e) {
  if (e.is_dir) return 'Folder';
  const dot = e.name.lastIndexOf('.');
  return dot > 0 ? `${e.name.slice(dot + 1).toUpperCase()} file` : 'File';
}

function setStatus(text, kind) {
  $('statusLeft').textContent = text;
  document.querySelector('.status').classList.toggle('error', kind === 'error');
}

function statusForImage() {
  if (!state.image) {
    setStatus('No image');
    $('statusRight').textContent = '';
    return;
  }
  const i = state.image.info;
  const what = i.floppy_format || `${i.fat_type} volume`;
  const part = i.partition ? `, partition ${i.partition.index}` : '';
  setStatus(`${state.image.name} — ${what}${part}${i.label ? ` — ${i.label}` : ''}`);
  $('statusRight').textContent = `${human(i.free_bytes)} free of ${human(i.total_bytes)}`;
}

async function showError(e) {
  const msg = e && e.message ? e.message : String(e);
  setStatus(msg, 'error');
  await dialog.message(msg, { title: 'SectorZero', kind: 'error' });
}

// Run an action with the busy indicator; errors become a dialog.
async function run(fn) {
  if (state.busy) return;
  state.busy = true;
  document.querySelector('.status').classList.add('busy');
  updateButtons();
  try {
    await fn();
  } catch (e) {
    await showError(e);
  } finally {
    state.busy = false;
    document.querySelector('.status').classList.remove('busy');
    updateButtons();
  }
}

// Call `fn(overwrite)`; if it fails because something exists, offer to overwrite.
async function withOverwrite(fn) {
  try {
    return await fn(false);
  } catch (e) {
    if (e && e.kind === 'exists') {
      const yes = await dialog.ask(`${e.message}\n\nOverwrite existing files?`, { title: 'SectorZero', kind: 'warning' });
      if (yes) return fn(true);
      return null;
    }
    throw e;
  }
}

function askText(title, initial = '') {
  return new Promise((resolve) => {
    const dlg = $('textDlg');
    $('textTitle').textContent = title;
    const input = $('textInput');
    input.value = initial;
    dlg.onclose = () => resolve(dlg.returnValue === 'ok' && input.value.trim() ? input.value.trim() : null);
    dlg.returnValue = '';
    dlg.showModal();
    input.select();
  });
}

// ---------- image lifecycle ----------

async function openImage(path) {
  state.image = await invoke('open_image', { path, partition: null });
  state.dir = '';
  await refresh();
}

async function refreshInfo() {
  state.image.info = await invoke('image_info');
}

async function refresh(keepSelection = false) {
  if (!state.image) return render();
  state.entries = await invoke('list_dir', { dir: state.dir });
  if (!keepSelection) {
    state.selected.clear();
    state.anchor = null;
  } else {
    const live = new Set(state.entries.map((e) => e.path));
    for (const p of [...state.selected]) if (!live.has(p)) state.selected.delete(p);
  }
  render();
}

async function closeImage() {
  await invoke('close_image');
  Object.assign(state, { image: null, dir: '', entries: [], anchor: null });
  state.selected.clear();
  render();
}

// ---------- actions ----------

async function actionOpen() {
  const path = await dialog.open({ title: 'Open disk image', multiple: false, filters: IMAGE_FILTERS });
  if (path) await openImage(path);
}

let formatsLoaded = false;
async function actionNew() {
  if (!formatsLoaded) {
    const formats = await invoke('formats');
    const sel = $('newFormat');
    for (const f of formats) {
      const o = document.createElement('option');
      o.value = f.id;
      o.textContent = f.name;
      if (f.id === '1440') o.selected = true;
      sel.append(o);
    }
    formatsLoaded = true;
  }
  const dlg = $('newDlg');
  const chosen = await new Promise((resolve) => {
    dlg.onclose = () => resolve(dlg.returnValue === 'ok');
    dlg.returnValue = '';
    dlg.showModal();
  });
  if (!chosen) return;
  const path = await dialog.save({ title: 'Save new image as', defaultPath: 'disk.ima', filters: IMAGE_FILTERS });
  if (!path) return;
  state.image = await invoke('new_image', { path, format: $('newFormat').value, label: $('newLabel').value || null });
  state.dir = '';
  await refresh();
}

async function injectPaths(paths) {
  if (!paths || paths.length === 0) return;
  const stats = await withOverwrite((overwrite) => invoke('inject', { paths, dest: state.dir, overwrite }));
  await refreshInfo();
  await refresh(true);
  if (stats) setStatus(`Injected ${stats.files} file(s), ${stats.dirs} folder(s), ${fmtBytes(stats.bytes)} bytes`);
}

async function actionInject() {
  const paths = await dialog.open({ title: 'Files to inject', multiple: true });
  if (paths) await injectPaths(Array.isArray(paths) ? paths : [paths]);
}

async function actionInjectDir() {
  const path = await dialog.open({ title: 'Folder to inject', directory: true, multiple: false });
  if (path) await injectPaths([path]);
}

async function actionExtract() {
  const dest = await dialog.open({ title: 'Extract to folder', directory: true, multiple: false });
  if (!dest) return;
  // Nothing selected: extract the current folder's contents (the whole image at the root).
  const paths = state.selected.size ? [...state.selected] : state.entries.map((e) => e.path);
  const all = state.dir === '' && state.selected.size === 0;
  const stats = await withOverwrite((overwrite) => invoke('extract', { paths: all ? [] : paths, dest, overwrite }));
  if (stats) setStatus(`Extracted ${stats.files} file(s), ${stats.dirs} folder(s), ${fmtBytes(stats.bytes)} bytes to ${dest}`);
}

async function actionDelete() {
  if (!state.selected.size) return;
  const items = [...state.selected];
  const label = items.length === 1 ? `"${baseName(items[0])}"` : `${items.length} items`;
  const yes = await dialog.ask(`Delete ${label} from the image? Folders are deleted with everything in them.`, {
    title: 'Delete', kind: 'warning',
  });
  if (!yes) return;
  const stats = await invoke('delete', { paths: items });
  await refreshInfo();
  await refresh();
  setStatus(`Deleted ${stats.files} file(s), ${stats.dirs} folder(s)`);
}

async function actionMkdir() {
  const name = await askText('New folder name');
  if (!name) return;
  await invoke('make_dir', { path: join(state.dir, name) });
  await refresh(true);
}

async function actionRename() {
  if (state.selected.size !== 1) return;
  const from = [...state.selected][0];
  const name = await askText('Rename to', baseName(from));
  if (!name || name === baseName(from)) return;
  await invoke('rename', { from, to: join(state.dir, name) });
  await refresh();
  state.selected.add(join(state.dir, name));
  render();
}

async function actionInfo() {
  await refreshInfo();
  const i = state.image.info;
  const rows = [
    ['File', state.image.path],
    ['Layout', i.partition ? `MBR, partition ${i.partition.index} (${i.partition.type_name})` : 'Bare volume'],
    ['File system', i.fat_type],
    ['Format', i.floppy_format || '—'],
    ['Label', i.label || '(none)'],
    ['Serial number', i.volume_id],
    ['OEM name', i.oem_name],
    ['Sector size', `${i.bytes_per_sector} bytes`],
    ['Cluster size', `${fmtBytes(i.cluster_size)} bytes`],
    ['Capacity', `${fmtBytes(i.total_bytes)} bytes (${fmtBytes(i.total_clusters)} clusters)`],
    ['Free', `${fmtBytes(i.free_bytes)} bytes (${fmtBytes(i.free_clusters)} clusters)`],
  ];
  const dl = $('infoList');
  dl.replaceChildren(...rows.flatMap(([k, v]) => {
    const dt = document.createElement('dt');
    dt.textContent = k;
    const dd = document.createElement('dd');
    dd.textContent = v;
    return [dt, dd];
  }));
  $('infoDlg').showModal();
}

function enter(dir) {
  state.dir = dir;
  return refresh();
}

// ---------- rendering ----------

function updateButtons() {
  const has = !!state.image;
  document.querySelectorAll('[data-needs-image]').forEach((b) => { b.disabled = !has || state.busy; });
  document.querySelectorAll('[data-needs-selection]').forEach((b) => { b.disabled = !has || state.busy || state.selected.size === 0; });
  $('btnRename').disabled ||= state.selected.size !== 1;
  $('btnNew').disabled = state.busy;
  $('btnOpen').disabled = state.busy;
}

function renderCrumbs() {
  const nav = $('crumbs');
  nav.replaceChildren();
  if (!state.image) return;
  const parts = state.dir ? state.dir.split('/') : [];
  const mk = (label, dir) => {
    const b = document.createElement('button');
    b.textContent = label;
    b.onclick = () => run(() => enter(dir));
    return b;
  };
  nav.append(mk(state.image.name, ''));
  parts.forEach((p, i) => {
    const s = document.createElement('span');
    s.className = 'sep';
    s.textContent = '›';
    nav.append(s, mk(p, parts.slice(0, i + 1).join('/')));
  });
}

function row(cells, cls) {
  const tr = document.createElement('tr');
  if (cls) tr.className = cls;
  for (const [text, c] of cells) {
    const td = document.createElement('td');
    td.className = c;
    if (text instanceof Node) td.append(text); else td.textContent = text;
    tr.append(td);
  }
  return tr;
}

function nameCell(e) {
  const frag = document.createDocumentFragment();
  const icon = document.createElement('span');
  icon.className = 'icon';
  icon.textContent = e.is_dir ? '📁' : '📄';
  frag.append(icon, e.name);
  if (e.short_name && e.short_name !== e.name) {
    const s = document.createElement('span');
    s.className = 'short';
    s.textContent = e.short_name;
    frag.append(s);
  }
  return frag;
}

function render() {
  const has = !!state.image;
  $('empty').hidden = has;
  $('files').hidden = !has;
  renderCrumbs();
  statusForImage();
  const tbody = $('rows');
  tbody.replaceChildren();
  if (has) {
    if (state.dir) {
      const up = row([['📁 ..', 'c-name'], ['', 'c-size'], ['Parent folder', 'c-type'], ['', 'c-date'], ['', 'c-attr']]);
      up.ondblclick = () => run(() => enter(parent(state.dir)));
      tbody.append(up);
    }
    state.entries.forEach((e, idx) => {
      const flags = e.attributes;
      const attr = (flags.read_only ? 'R' : '') + (flags.hidden ? 'H' : '') + (flags.system ? 'S' : '') + (flags.archive ? 'A' : '');
      const tr = row([
        [nameCell(e), 'c-name'],
        [e.is_dir ? '' : fmtBytes(e.size), 'c-size'],
        [typeOf(e), 'c-type'],
        [e.modified, 'c-date'],
        [attr, 'c-attr'],
      ], state.selected.has(e.path) ? 'selected' : '');
      tr.title = e.path;
      tr.onclick = (ev) => select(idx, ev);
      tr.ondblclick = () => { if (e.is_dir) run(() => enter(e.path)); };
      tbody.append(tr);
    });
  }
  updateButtons();
}

function select(idx, ev) {
  const path = state.entries[idx].path;
  if (ev.shiftKey && state.anchor !== null) {
    const [a, b] = [Math.min(state.anchor, idx), Math.max(state.anchor, idx)];
    if (!ev.ctrlKey) state.selected.clear();
    for (let i = a; i <= b; i++) state.selected.add(state.entries[i].path);
  } else if (ev.ctrlKey) {
    state.selected.has(path) ? state.selected.delete(path) : state.selected.add(path);
    state.anchor = idx;
  } else {
    state.selected.clear();
    state.selected.add(path);
    state.anchor = idx;
  }
  render();
}

// ---------- wiring ----------

$('btnNew').onclick = () => run(actionNew);
$('btnOpen').onclick = () => run(actionOpen);
$('btnClose').onclick = () => run(closeImage);
$('btnInject').onclick = () => run(actionInject);
$('btnInjectDir').onclick = () => run(actionInjectDir);
$('btnExtract').onclick = () => run(actionExtract);
$('btnMkdir').onclick = () => run(actionMkdir);
$('btnRename').onclick = () => run(actionRename);
$('btnDelete').onclick = () => run(actionDelete);
$('btnInfo').onclick = () => run(actionInfo);

document.addEventListener('keydown', (ev) => {
  if (document.querySelector('dialog[open]')) return;
  const k = ev.key;
  if (ev.ctrlKey && k.toLowerCase() === 'o') { ev.preventDefault(); run(actionOpen); }
  else if (ev.ctrlKey && k.toLowerCase() === 'n') { ev.preventDefault(); run(actionNew); }
  else if (!state.image) return;
  else if (ev.ctrlKey && k.toLowerCase() === 'a') {
    ev.preventDefault();
    state.entries.forEach((e) => state.selected.add(e.path));
    render();
  } else if (k === 'Delete') run(actionDelete);
  else if (k === 'Insert') run(actionInject);
  else if (k === 'F2') run(actionRename);
  else if (k === 'Backspace' && state.dir) run(() => enter(parent(state.dir)));
  else if (k === 'Enter' && state.selected.size === 1) {
    const e = state.entries.find((x) => state.selected.has(x.path));
    if (e && e.is_dir) run(() => enter(e.path));
  }
});

// Files dropped from Explorer: open an image, or inject into the current folder.
getCurrentWebview().onDragDropEvent((event) => {
  const p = event.payload;
  const zone = $('dropzone');
  if (p.type === 'enter' || p.type === 'over') {
    const opening = !state.image;
    $('dropText').textContent = opening ? 'Drop to open the image' : `Drop to inject into /${state.dir}`;
    zone.hidden = false;
  } else if (p.type === 'leave') {
    zone.hidden = true;
  } else if (p.type === 'drop') {
    zone.hidden = true;
    const paths = p.paths || [];
    if (!state.image) {
      const img = paths.find(isImageName) || paths[0];
      if (img) run(() => openImage(img));
    } else {
      run(() => injectPaths(paths));
    }
  }
});

render();
