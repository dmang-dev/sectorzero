// SectorZero UI. Talks to the Rust side through Tauri commands (see src-tauri/src/main.rs).
'use strict';

const { invoke } = window.__TAURI__.core;
const dialog = window.__TAURI__.dialog;
const { getCurrentWebview } = window.__TAURI__.webview;

const IMAGE_EXTS = ['ima', 'img', 'vfd', 'flp', 'dsk', 'imz', 'wlz', 'vhd', 'vhdx', 'vmdk', 'iso'];
const IMAGE_FILTERS = [
  { name: 'Disk images', extensions: IMAGE_EXTS },
  { name: 'All files', extensions: ['*'] },
];
const CONTAINER_EXT = { raw: 'ima', imz: 'imz', vhd: 'vhd', vhdx: 'vhdx', vmdk: 'vmdk' };
const filterFor = (container) => [
  { name: container === 'raw' ? 'Raw image' : container.toUpperCase(), extensions: container === 'raw' ? ['ima', 'img', 'vfd', 'flp'] : [CONTAINER_EXT[container]] },
];

const state = {
  image: null,       // { path, name, info }
  partition: null,   // selected partition number, null = default
  dir: '',           // current folder inside the image, '' = root
  entries: [],
  selected: new Set(), // entry paths
  anchor: null,      // index for shift-click ranges
  busy: false,
  formats: null,
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
const stem = (p) => baseName(p).replace(/\.[^.]*$/, '');
const isImageName = (p) => IMAGE_EXTS.includes(baseName(p).split('.').pop().toLowerCase());
const writable = () => !!state.image && state.image.info.fs_writable;

function typeOf(e) {
  if (e.is_dir) return 'Folder';
  const dot = e.name.lastIndexOf('.');
  return dot > 0 ? `${e.name.slice(dot + 1).toUpperCase()} file` : 'File';
}

function setStatus(text, kind) {
  $('statusLeft').textContent = text;
  document.querySelector('.status').classList.toggle('error', kind === 'error');
}

function layoutName(i) {
  const l = i.layout;
  if (l.kind === 'Mbr' || l.kind === 'Gpt') return `${l.kind.toUpperCase()}${i.partition ? `, partition ${i.partition.index} (${i.partition.type_name})` : ''}`;
  if (l.kind === 'Iso') return 'CD/DVD image';
  return 'Bare volume';
}

function statusForImage() {
  if (!state.image) {
    setStatus('No image');
    $('statusRight').textContent = '';
    return;
  }
  const i = state.image.info;
  const what = i.floppy_format || `${i.fs_type} volume`;
  const part = i.partition ? `, partition ${i.partition.index}` : '';
  const ro = i.fs_writable ? '' : ' (read-only)';
  setStatus(`${state.image.name} — ${what}${part}${i.label ? ` — ${i.label}` : ''}${ro}`);
  $('statusRight').textContent = i.fs_writable
    ? `${human(i.free_bytes)} free of ${human(i.total_bytes)}`
    : human(i.total_bytes);
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

// Show a <dialog>; resolves with the button value that closed it.
function showDialog(dlg) {
  return new Promise((resolve) => {
    dlg.onclose = () => resolve(dlg.returnValue);
    dlg.returnValue = '';
    dlg.showModal();
  });
}

async function askText(title, initial = '', { allowEmpty = false, hint = '' } = {}) {
  const input = $('textInput');
  $('textTitle').textContent = title;
  $('textHint').textContent = hint;
  input.value = initial;
  const p = showDialog($('textDlg'));
  input.select();
  const v = await p;
  if (v !== 'ok') return null;
  const t = input.value.trim();
  return t || allowEmpty ? t : null;
}

function fillList(dl, rows) {
  dl.replaceChildren(...rows.flatMap(([k, v]) => {
    const dt = document.createElement('dt');
    dt.textContent = k;
    const dd = document.createElement('dd');
    dd.textContent = v;
    return [dt, dd];
  }));
}

async function loadFormats() {
  if (!state.formats) state.formats = await invoke('formats');
  return state.formats;
}

function fillFormatSelect(sel, { withDisk = false, selected = '1440' } = {}) {
  sel.replaceChildren();
  for (const f of state.formats) {
    const o = document.createElement('option');
    o.value = f.id;
    o.textContent = f.name;
    if (f.id === selected) o.selected = true;
    sel.append(o);
  }
  if (withDisk) {
    const o = document.createElement('option');
    o.value = 'disk';
    o.textContent = 'Hard disk / volume image (size below)';
    sel.append(o);
  }
}

// ---------- image lifecycle ----------

async function openImage(path, partition = null) {
  state.image = await invoke('open_image', { path, partition, member: null });
  state.partition = partition;
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
  Object.assign(state, { image: null, partition: null, dir: '', entries: [], anchor: null });
  state.selected.clear();
  render();
}

// ---------- actions ----------

async function actionOpen() {
  const path = await dialog.open({ title: 'Open disk image', multiple: false, filters: IMAGE_FILTERS });
  if (path) await openImage(path);
}

function syncNewDialog() {
  const kind = document.querySelector('input[name="newKind"]:checked').value;
  document.querySelectorAll('#newDlg [data-kind]').forEach((el) => { el.hidden = el.dataset.kind !== kind; });
  const c = $('newContainer').value;
  $('newDynamic').disabled = !['vhd', 'vhdx', 'vmdk'].includes(c);
}

async function actionNew() {
  await loadFormats();
  if (!$('newFormat').options.length) fillFormatSelect($('newFormat'));
  syncNewDialog();
  if (await showDialog($('newDlg')) !== 'ok') return;
  const kind = document.querySelector('input[name="newKind"]:checked').value;
  const container = $('newContainer').value;
  const ext = CONTAINER_EXT[container] === 'ima' && kind === 'disk' ? 'img' : CONTAINER_EXT[container];
  const path = await dialog.save({ title: 'Save new image as', defaultPath: `disk.${ext}`, filters: filterFor(container) });
  if (!path) return;
  const spec = {
    path,
    format: kind === 'floppy' ? $('newFormat').value : null,
    size_mb: kind === 'disk' ? Number($('newSize').value) : null,
    label: $('newLabel').value || null,
    scheme: kind === 'disk' ? $('newScheme').value : 'none',
    container,
    dynamic: !$('newDynamic').disabled && $('newDynamic').checked,
  };
  state.image = await invoke('new_image', { spec });
  state.partition = null;
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

async function actionLabel() {
  const label = await askText('Volume label', state.image.info.label, {
    allowEmpty: true, hint: 'Up to 11 characters. Leave empty to remove the label.',
  });
  if (label === null) return;
  const keep = state.image.path;
  state.image = await invoke('set_label', { label: label || null });
  state.image.path = keep;
  render();
  setStatus(label ? `Label set to ${label.toUpperCase()}` : 'Label removed');
}

function bootRows(b) {
  return [
    ['File system', b.fat_type],
    ['Jump', b.jump],
    ['Bytes per sector', b.bytes_per_sector],
    ['Sectors per cluster', b.sectors_per_cluster],
    ['Reserved sectors', b.reserved_sectors],
    ['FATs', b.fats],
    ['Root entries', b.root_entries],
    ['Total sectors', fmtBytes(b.total_sectors)],
    ['Media', b.media],
    ['Sectors per FAT', b.sectors_per_fat],
    ['Label (boot sector)', b.label ?? '—'],
    ['FS type field', b.fs_type ?? '—'],
    ['Boot code', `${b.has_boot_code ? 'present' : 'none'}${b.signature_55aa ? '' : ', no 55AA signature'}`],
  ].map(([k, v]) => [k, String(v)]);
}

function fillBootForm(b) {
  fillList($('bootList'), bootRows(b));
  $('bootOem').value = b.oem_name.trimEnd();
  $('bootSerial').value = b.volume_id ?? '';
  $('bootSerial').disabled = !b.volume_id;
  $('bootSpt').value = b.sectors_per_track;
  $('bootHeads').value = b.heads;
  $('bootHidden').value = b.hidden_sectors;
  $('bootDrive').value = b.drive_number;
}

async function actionBoot() {
  let b = await invoke('boot_info');
  for (;;) {
    fillBootForm(b);
    const v = await showDialog($('bootDlg'));
    if (v === 'export') {
      const dest = await dialog.save({ title: 'Save boot sector', defaultPath: `${stem(state.image.name)}.bin`, filters: [{ name: 'Boot sector', extensions: ['bin', 'bs'] }] });
      if (dest) { await invoke('export_boot', { dest }); setStatus(`Boot sector saved to ${dest}`); }
    } else if (v === 'import') {
      const src = await dialog.open({ title: 'Boot sector file', multiple: false, filters: [{ name: 'Boot sector', extensions: ['bin', 'bs', 'ima', 'img'] }, { name: 'All files', extensions: ['*'] }] });
      if (src) { b = await invoke('import_boot', { src }); setStatus('Boot code imported (disk geometry kept)'); }
    } else if (v === 'save') {
      const num = (id) => ($(id).value === '' ? null : Number($(id).value));
      const drive = $('bootDrive').value.trim();
      const edit = {
        oem_name: $('bootOem').value,
        volume_id: $('bootSerial').disabled ? null : $('bootSerial').value.trim() || null,
        sectors_per_track: num('bootSpt'),
        heads: num('bootHeads'),
        hidden_sectors: num('bootHidden'),
        drive_number: drive ? parseInt(drive, drive.toLowerCase().startsWith('0x') ? 16 : 10) : null,
      };
      b = await invoke('edit_boot', { edit });
      await refreshInfo();
      render();
      setStatus('Boot sector saved');
      return;
    } else {
      return;
    }
  }
}

function defragRows(r, analysed) {
  const rows = [
    ['Files / folders', `${r.files} / ${r.dirs}`],
    ['Fragmented', `${r.fragmented} (${r.fragments} extents)`],
    ['Used clusters', fmtBytes(r.used_clusters)],
    ['Free clusters', fmtBytes(r.free_clusters)],
    ['Cluster size', `${fmtBytes(r.cluster_size)} bytes`],
  ];
  if (r.lost_clusters) rows.push(['Lost clusters', `${r.lost_clusters} (will be freed)`]);
  if (!analysed) rows.push(['Moved', `${fmtBytes(r.moved_clusters)} clusters`]);
  return rows;
}

async function actionDefrag() {
  const r = await invoke('defrag', { analyze: true, zeroFree: false });
  fillList($('defragList'), defragRows(r, true));
  $('defragGo').textContent = r.fragmented ? 'Defragment' : 'Defragment anyway';
  if (await showDialog($('defragDlg')) !== 'ok') return;
  const done = await invoke('defrag', { analyze: false, zeroFree: $('defragZero').checked });
  await refreshInfo();
  await refresh(true);
  setStatus(`Defragmented: ${done.moved_clusters} cluster(s) moved, ${done.fragments} extent(s) now`);
}

async function actionFormat() {
  await loadFormats();
  fillFormatSelect($('formatSel'), { withDisk: true, selected: '' });
  const sync = () => { $('formatSizeWrap').hidden = $('formatSel').value !== 'disk'; };
  $('formatSel').onchange = sync;
  $('formatSize').value = Math.max(1, Math.round(state.image.info.volume_size / 1048576));
  sync();
  if (await showDialog($('formatDlg')) !== 'ok') return;
  const disk = $('formatSel').value === 'disk';
  const keep = state.image.path;
  state.image = await invoke('change_format', {
    format: disk ? null : $('formatSel').value,
    sizeMb: disk ? Number($('formatSize').value) : null,
  });
  state.image.path = keep;
  state.dir = '';
  await refresh();
  setStatus(`Changed format to ${state.image.info.floppy_format || human(state.image.info.volume_size)}`);
}

async function actionSaveAs() {
  const sync = () => { $('saveDynamic').disabled = !['vhd', 'vhdx', 'vmdk'].includes($('saveContainer').value); };
  $('saveContainer').onchange = sync;
  sync();
  if (await showDialog($('saveAsDlg')) !== 'ok') return;
  const container = $('saveContainer').value;
  const dest = await dialog.save({ title: 'Save a copy as', defaultPath: `${stem(state.image.name)}.${CONTAINER_EXT[container]}`, filters: filterFor(container) });
  if (!dest) return;
  const r = await invoke('save_as', { dest, container, dynamic: !$('saveDynamic').disabled && $('saveDynamic').checked });
  setStatus(`Saved ${baseName(dest)} (${human(r.bytes)}, ${human(r.stored)} of data)`);
  const open = await dialog.ask(`Saved ${baseName(dest)}. Open it now?`, { title: 'Save as', kind: 'info' });
  if (open) await openImage(dest);
}

async function actionIso() {
  $('isoBootPick').onclick = async () => {
    const p = await dialog.open({ title: 'Boot image', multiple: false, filters: [{ name: 'Boot images', extensions: ['ima', 'img', 'bin', 'efi'] }, { name: 'All files', extensions: ['*'] }] });
    if (p) $('isoBoot').value = p;
  };
  $('isoBootClear').onclick = () => { $('isoBoot').value = ''; };
  if (await showDialog($('isoDlg')) !== 'ok') return;
  const folder = await dialog.open({ title: 'Folder to put on the CD', directory: true, multiple: false });
  if (!folder) return;
  const dest = await dialog.save({ title: 'Save ISO image as', defaultPath: `${baseName(folder) || 'cd'}.iso`, filters: [{ name: 'ISO image', extensions: ['iso'] }] });
  if (!dest) return;
  const r = await invoke('make_iso', {
    folder, dest, label: $('isoLabel').value, boot: $('isoBoot').value || null, efi: $('isoEfi').checked,
  });
  setStatus(`Built ${baseName(dest)}: ${r.files} file(s), ${human(r.image_size)}${r.bootable ? ', bootable' : ''}`);
  const open = await dialog.ask(`Built ${baseName(dest)}. Open it now?`, { title: 'Build ISO', kind: 'info' });
  if (open) await openImage(dest);
}

async function actionBootImage() {
  const entries = state.image.info.iso?.boot_entries ?? [];
  if (!entries.length) {
    await dialog.message('This ISO has no El Torito boot catalog.', { title: 'Boot image' });
    return;
  }
  let entry = entries[0];
  if (entries.length > 1) {
    const list = entries.map((e) => `${e.index}: ${e.platform}, ${e.media}, ${human(e.image_size)}`).join('\n');
    const n = await askText('Boot entry number', '1', { hint: list });
    if (!n) return;
    entry = entries.find((e) => e.index === Number(n));
    if (!entry) throw new Error(`There is no boot entry ${n}.`);
  }
  const floppy = entry.media.includes('floppy');
  const dest = await dialog.save({ title: 'Save boot image', defaultPath: `${stem(state.image.name)}-boot.${floppy ? 'ima' : 'img'}` });
  if (!dest) return;
  const n = await invoke('export_boot_image', { entry: entry.index, dest });
  setStatus(`Boot image saved to ${dest} (${fmtBytes(n)} bytes)`);
}

async function actionInfo() {
  await refreshInfo();
  const i = state.image.info;
  const c = i.container;
  const rows = [
    ['File', state.image.path],
    ['Container', `${c.kind.toUpperCase()} (${c.variant})${c.member ? `, member ${c.member}` : ''}`],
    ['Disk size', `${fmtBytes(c.virtual_size)} bytes`],
    ['Layout', layoutName(i)],
    ['File system', i.fs_type + (i.fs_writable ? '' : ' (read-only)')],
  ];
  if (c.comment) rows.push(['Comment', c.comment]);
  if (i.iso) {
    rows.push(['Volume ID', i.label || '(none)']);
    if (i.iso.application) rows.push(['Application', i.iso.application]);
    if (i.iso.publisher) rows.push(['Publisher', i.iso.publisher]);
    if (i.iso.created) rows.push(['Created', i.iso.created]);
    rows.push(['Size', `${fmtBytes(i.total_bytes)} bytes`]);
    for (const b of i.iso.boot_entries) rows.push([`Boot entry ${b.index}`, `${b.platform}, ${b.media}, ${fmtBytes(b.image_size)} bytes`]);
  } else {
    rows.push(
      ['Format', i.floppy_format || '—'],
      ['Label', i.label || '(none)'],
      ['Serial number', i.volume_id],
      ['OEM name', i.oem_name],
      ['Sector size', `${i.bytes_per_sector} bytes`],
      ['Cluster size', `${fmtBytes(i.cluster_size)} bytes`],
      ['Capacity', `${fmtBytes(i.total_bytes)} bytes (${fmtBytes(i.total_clusters)} clusters)`],
      ['Free', `${fmtBytes(i.free_bytes)} bytes (${fmtBytes(i.free_clusters)} clusters)`],
    );
  }
  for (const p of i.layout.partitions ?? []) {
    rows.push([`Partition ${p.index}`, `${p.type_name}${p.name ? ` "${p.name}"` : ''}, ${human(p.sectors * p.sector_size)} at sector ${fmtBytes(p.start_lba)}, ${p.fs ?? 'no known file system'}`]);
  }
  fillList($('infoList'), rows);
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
  document.querySelectorAll('[data-needs-writable]').forEach((b) => { b.disabled = !writable() || state.busy; });
  document.querySelectorAll('[data-needs-selection]').forEach((b) => { b.disabled ||= state.selected.size === 0; });
  $('btnRename').disabled ||= state.selected.size !== 1;
  for (const id of ['btnNew', 'btnOpen', 'btnIso']) $(id).disabled = state.busy;
  const iso = has && state.image.info.iso;
  $('btnBootImage').hidden = !iso;
  for (const id of ['btnLabel', 'btnBoot', 'btnDefrag', 'btnFormat']) $(id).hidden = !!iso;
  $('btnFormat').disabled ||= !has || state.image.info.layout.kind !== 'BareVolume';
  $('partSel').disabled = state.busy;
}

function renderPartitions() {
  const parts = state.image?.info.layout.partitions ?? [];
  $('partWrap').hidden = parts.length < 2;
  const sel = $('partSel');
  sel.replaceChildren();
  for (const p of parts) {
    const o = document.createElement('option');
    o.value = p.index;
    o.textContent = `${p.index}: ${p.type_name}${p.name ? ` "${p.name}"` : ''} — ${human(p.sectors * p.sector_size)}${p.fs ? '' : ' (unreadable)'}`;
    o.selected = state.image.info.partition?.index === p.index;
    sel.append(o);
  }
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
  renderPartitions();
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
$('btnLabel').onclick = () => run(actionLabel);
$('btnBoot').onclick = () => run(actionBoot);
$('btnDefrag').onclick = () => run(actionDefrag);
$('btnFormat').onclick = () => run(actionFormat);
$('btnBootImage').onclick = () => run(actionBootImage);
$('btnSaveAs').onclick = () => run(actionSaveAs);
$('btnIso').onclick = () => run(actionIso);
$('btnInfo').onclick = () => run(actionInfo);
$('partSel').onchange = () => run(() => openImage(state.image.path, Number($('partSel').value)));
document.querySelectorAll('input[name="newKind"]').forEach((r) => { r.onchange = syncNewDialog; });
$('newContainer').onchange = syncNewDialog;

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
  } else if (k === 'Delete' && writable()) run(actionDelete);
  else if (k === 'Insert' && writable()) run(actionInject);
  else if (k === 'F2' && writable()) run(actionRename);
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
  const opening = !state.image || !writable();
  if (p.type === 'enter' || p.type === 'over') {
    $('dropText').textContent = opening ? 'Drop to open the image' : `Drop to inject into /${state.dir}`;
    zone.hidden = false;
  } else if (p.type === 'leave') {
    zone.hidden = true;
  } else if (p.type === 'drop') {
    zone.hidden = true;
    const paths = p.paths || [];
    if (opening) {
      const img = paths.find(isImageName) || paths[0];
      if (img) run(() => openImage(img));
    } else {
      run(() => injectPaths(paths));
    }
  }
});

render();
