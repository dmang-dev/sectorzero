// SectorZero UI. Talks to the Rust side through Tauri commands (see src-tauri/src/main.rs).
'use strict';

const { invoke } = window.__TAURI__.core;
const dialog = window.__TAURI__.dialog;
const { getCurrentWebview } = window.__TAURI__.webview;

const IMAGE_EXTS = ['ima', 'img', 'vfd', 'flp', 'dsk', 'imz', 'wlz', 'vhd', 'vud', 'vhdx', 'vmdk', 'vdi', 'qcow2', '001',
  'nhd', 'hdi', 'fdi', 'iso', 'cue', 'bin', 'nrg', 'mds', 'mdf'];
const IMAGE_FILTERS = [
  { name: 'Disk images', extensions: IMAGE_EXTS },
  { name: 'All files', extensions: ['*'] },
];
const CONTAINER_EXT = {
  raw: 'ima', imz: 'imz', vhd: 'vhd', vhdx: 'vhdx', vmdk: 'vmdk', vdi: 'vdi', qcow2: 'qcow2', nhd: 'nhd', hdi: 'hdi', fdi: 'fdi',
};
const DYNAMIC = ['vhd', 'vhdx', 'vmdk', 'vdi'];
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
  $('statusRight').textContent = i.iso
    ? human(i.total_bytes)
    : `${human(i.free_bytes)} free of ${human(i.total_bytes)}`;
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

// ---------- custom geometry fields (New and Change format dialogs) ----------

const hex = (n) => `0x${n.toString(16).toUpperCase().padStart(2, '0')}`;
const parseNum = (s, dflt) => {
  const t = String(s).trim();
  if (!t) return dflt;
  const n = t.toLowerCase().startsWith('0x') ? parseInt(t, 16) : Number(t);
  if (!Number.isFinite(n)) throw new Error(`"${t}" is not a number.`);
  return n;
};

// Defaults match `s0 new --sectors`: 32 sectors/track, 64 heads, media F8, 2 FATs, drive 80h.
const GEO_DEFAULT = {
  total_sectors: 131072, sectors_per_track: 32, heads: 64, bytes_per_cluster: 0, root_entries: 0,
  fats: 2, media: 0xF8, fat_bits: 0, hidden_sectors: 0, drive_number: 0x80,
};

function geoInit(box) {
  if (box.firstChild) return;
  box.append($('geoTpl').content.cloneNode(true));
  const f = (n) => box.querySelector(`[name="${n}"]`);
  const showSize = () => { f('size').value = human((Number(f('total_sectors').value) || 0) * 512); };
  f('total_sectors').oninput = showSize;
  // The dialog's action is already running (state.busy), so this can't go through run().
  box.querySelector('[data-geo-like]').onclick = async () => {
    try {
      const p = await dialog.open({
        title: 'Copy the layout of', multiple: false,
        filters: [{ name: 'Images and boot sectors', extensions: [...IMAGE_EXTS, 'bin', 'bs'] }, { name: 'All files', extensions: ['*'] }],
      });
      if (p) geoFill(box, await invoke('geometry_of', { path: p }));
    } catch (e) {
      await showError(e);
    }
  };
  geoFill(box, GEO_DEFAULT);
}

function geoFill(box, g) {
  const f = (n) => box.querySelector(`[name="${n}"]`);
  for (const k of ['total_sectors', 'sectors_per_track', 'heads', 'hidden_sectors']) f(k).value = g[k];
  f('root_entries').value = g.root_entries || '';
  f('bytes_per_cluster').value = String(g.bytes_per_cluster);
  f('fat_bits').value = String(g.fat_bits);
  f('fats').value = String(g.fats);
  f('media').value = hex(g.media);
  f('drive_number').value = hex(g.drive_number);
  f('total_sectors').oninput();
}

function geoRead(box) {
  const f = (n) => box.querySelector(`[name="${n}"]`).value;
  return {
    total_sectors: parseNum(f('total_sectors')),
    sectors_per_track: parseNum(f('sectors_per_track')),
    heads: parseNum(f('heads')),
    bytes_per_cluster: Number(f('bytes_per_cluster')),
    root_entries: parseNum(f('root_entries'), 0),
    fats: Number(f('fats')),
    media: parseNum(f('media'), 0xF8),
    fat_bits: Number(f('fat_bits')),
    hidden_sectors: parseNum(f('hidden_sectors'), 0),
    drive_number: parseNum(f('drive_number'), 0x80),
  };
}

// Hidden fields must not block the form's validation.
function geoEnable(box, on) {
  box.querySelectorAll('input, select, button').forEach((el) => { el.disabled = !on; });
}

function syncNewDialog() {
  const kind = document.querySelector('input[name="newKind"]:checked').value;
  document.querySelectorAll('#newDlg [data-kind]').forEach((el) => { el.hidden = el.dataset.kind !== kind; });
  geoEnable($('newGeo'), kind === 'custom');
  // Custom layouts are FAT boot sector parameters.
  $('newFsWrap').hidden = kind === 'custom';
  const ext = kind !== 'custom' && $('newFs').value !== 'fat';
  $('newLabel').maxLength = ext ? 16 : 11;
  $('newLabelText').textContent = `Volume label (optional, up to ${ext ? 16 : 11} characters)`;
  const c = $('newContainer').value;
  $('newDynamic').disabled = !DYNAMIC.includes(c);
}

async function actionNew() {
  await loadFormats();
  if (!$('newFormat').options.length) fillFormatSelect($('newFormat'));
  geoInit($('newGeo'));
  syncNewDialog();
  if (await showDialog($('newDlg')) !== 'ok') return;
  const kind = document.querySelector('input[name="newKind"]:checked').value;
  const geometry = kind === 'custom' ? geoRead($('newGeo')) : null;
  const container = $('newContainer').value;
  const ext = CONTAINER_EXT[container] === 'ima' && kind !== 'floppy' ? 'img' : CONTAINER_EXT[container];
  const path = await dialog.save({ title: 'Save new image as', defaultPath: `disk.${ext}`, filters: filterFor(container) });
  if (!path) return;
  const spec = {
    path,
    format: kind === 'floppy' ? $('newFormat').value : null,
    size_mb: kind === 'disk' ? Number($('newSize').value) : null,
    label: $('newLabel').value || null,
    scheme: kind === 'disk' ? $('newScheme').value : 'none',
    fs: kind === 'custom' ? 'fat' : $('newFs').value,
    container,
    dynamic: !$('newDynamic').disabled && $('newDynamic').checked,
    geometry,
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
  const o = document.createElement('option');
  o.value = 'custom';
  o.textContent = 'Custom layout (below)';
  $('formatSel').append(o);
  geoInit($('formatGeo'));
  try { geoFill($('formatGeo'), await invoke('geometry_of', { path: null })); } catch { /* keep defaults */ }
  const sync = () => {
    $('formatSizeWrap').hidden = $('formatSel').value !== 'disk';
    $('formatGeoWrap').hidden = $('formatSel').value !== 'custom';
    geoEnable($('formatGeo'), $('formatSel').value === 'custom');
  };
  $('formatSel').onchange = sync;
  $('formatSize').value = Math.max(1, Math.round(state.image.info.volume_size / 1048576));
  sync();
  if (await showDialog($('formatDlg')) !== 'ok') return;
  const choice = $('formatSel').value;
  const disk = choice === 'disk';
  const custom = choice === 'custom';
  const keep = state.image.path;
  state.image = await invoke('change_format', {
    format: disk || custom ? null : choice,
    sizeMb: disk ? Number($('formatSize').value) : null,
    geometry: custom ? geoRead($('formatGeo')) : null,
  });
  state.image.path = keep;
  state.dir = '';
  await refresh();
  setStatus(`Changed format to ${state.image.info.floppy_format || human(state.image.info.volume_size)}`);
}

async function actionSaveAs() {
  $('saveAsTitle').textContent = 'Save a copy as';
  const sync = () => { $('saveDynamic').disabled = !DYNAMIC.includes($('saveContainer').value); };
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

async function actionDiff() {
  const dest = await dialog.save({
    title: 'Create a differencing disk', defaultPath: `${stem(state.image.name)}-changes.vhd`,
    filters: [{ name: 'Differencing VHD', extensions: ['vhd'] }, { name: 'Virtual PC undo disk', extensions: ['vud'] }],
  });
  if (!dest) return;
  state.image = await invoke('make_differencing', { dest });
  state.partition = null;
  state.dir = '';
  await refresh();
  setStatus(`Created ${baseName(dest)}; changes now go there and ${baseName(state.image.info.container.parent || 'the parent')} stays as it is`);
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
  if (i.read_only_reason) rows.push(['Read-only because', i.read_only_reason]);
  if (c.comment) rows.push(['Comment', c.comment]);
  if (c.parent) rows.push(['Parent', c.parent]);
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
      [i.fs_type.startsWith('ext') ? 'UUID' : 'Serial number', i.volume_id],
      ['OEM name', i.oem_name || '—'],
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

const selectedEntries = () => state.entries.filter((e) => state.selected.has(e.path));
const toLocalInput = (s) => (s ? s.replace(' ', 'T') : '');

// What the Properties dialog can change: FAT has four attributes and three times
// (2-second modified steps, date-only access, 1980-2107); ext has read-only (the write
// permission bits) and full times; anything else (NTFS, ISO, read-only images) is shown only.
function propsMode() {
  const fs = state.image.info.fs_type || '';
  const kind = fs.startsWith('FAT') ? 'fat' : fs.startsWith('ext') ? 'ext' : 'other';
  return { kind, editable: writable() && kind !== 'other' };
}

function setupPropsDialog(mode, items) {
  const ext = mode.kind === 'ext';
  const fat = mode.kind === 'fat';
  for (const box of document.querySelectorAll('#propsAttrs input')) {
    const k = box.dataset.attr;
    box.disabled = !mode.editable || (ext && k !== 'read_only');
    box.parentElement.title = ext && k !== 'read_only'
      ? (k === 'hidden' ? 'Names starting with a dot are hidden on Linux' : 'ext has no such attribute')
      : '';
  }
  const range = fat ? ['1980-01-01', '2107-12-31'] : ['1902-01-01', '2446-12-31'];
  for (const id of ['propsModified', 'propsCreated', 'propsAccessed']) {
    const el = $(id);
    el.disabled = !mode.editable;
    if (id === 'propsAccessed') {
      const withTime = !fat && items.some((e) => e.accessed.includes(' '));
      el.type = fat || (!ext && !withTime) ? 'date' : 'datetime-local';
      $('propsAccessedText').textContent = el.type === 'date' ? 'Accessed (date only)' : 'Accessed';
    }
    const time = el.type === 'datetime-local';
    el.min = range[0] + (time ? 'T00:00' : '');
    el.max = range[1] + (time ? 'T23:59' : '');
    if (time) el.step = fat && id === 'propsModified' ? '2' : '1';
  }
  $('propsRecursive').disabled = !mode.editable;
  $('propsApply').hidden = !mode.editable;
  $('propsCancel').textContent = mode.editable ? 'Cancel' : 'Close';
  $('propsNote').textContent = !mode.editable
    ? (writable() || mode.kind === 'other'
      ? `${state.image.info.fs_type} volumes are read-only here; the values are shown for reference.`
      : 'The image is open read-only; the values are shown for reference.')
    : ext
      ? 'Only the fields you change are written. On ext, read-only removes the write permission bits (clearing it gives the owner write permission back); hidden, system and archive do not exist.'
      : 'Only the fields you change are written. FAT stores modified times in 2-second steps.';
}

async function actionProps() {
  const items = selectedEntries();
  if (!items.length) return;
  const mode = propsMode();
  setupPropsDialog(mode, items);
  const one = items.length === 1 ? items[0] : null;
  $('propsTitle').textContent = one ? `Properties of ${one.name}` : `Properties of ${items.length} items`;
  $('propsHint').textContent = one
    ? `${one.is_dir ? 'Folder' : `${fmtBytes(one.size)} bytes`}${one.short_name && one.short_name !== one.name ? ` — short name ${one.short_name}` : ''}`
    : 'Mixed values are shown as a dash; leave them alone to keep each item as it is.';
  const touched = new Set();
  for (const box of document.querySelectorAll('#propsAttrs input')) {
    const k = box.dataset.attr;
    const on = items.filter((e) => e.attributes[k]).length;
    box.checked = on === items.length;
    box.indeterminate = on > 0 && on < items.length;
    box.onchange = () => touched.add(k);
  }
  const dates = { propsModified: 'modified', propsCreated: 'created', propsAccessed: 'accessed' };
  for (const [id, k] of Object.entries(dates)) {
    const same = items.every((e) => e[k] === items[0][k]);
    $(id).value = same ? toLocalInput(items[0][k]) : '';
    $(id).oninput = () => touched.add(k);
  }
  const anyDir = items.some((e) => e.is_dir);
  $('propsRecursiveWrap').hidden = !anyDir;
  $('propsRecursive').checked = false;
  $('propsRecursive').onchange = () => touched.add('recursive');
  if (await showDialog($('propsDlg')) !== 'ok' || !mode.editable) return;
  const edit = {};
  for (const box of document.querySelectorAll('#propsAttrs input')) {
    if (touched.has(box.dataset.attr)) edit[box.dataset.attr] = box.checked;
  }
  for (const [id, k] of Object.entries(dates)) {
    if (touched.has(k) && $(id).value) edit[k] = $(id).value.replace('T', ' ');
  }
  const recursive = anyDir && $('propsRecursive').checked;
  if (!Object.keys(edit).length) return;
  const n = await invoke('edit_entries', { paths: items.map((e) => e.path), edit, recursive });
  await refresh(true);
  setStatus(`Updated ${n} item(s)`);
}

async function actionChecksum() {
  const file = selectedEntries().length === 1 && !selectedEntries()[0].is_dir ? selectedEntries()[0] : null;
  const fileOpt = $('sumWhat').querySelector('option[value="file"]');
  fileOpt.disabled = !file;
  fileOpt.textContent = file ? `The selected file (${file.name})` : 'The selected file (select one file first)';
  if (!file && $('sumWhat').value === 'file') $('sumWhat').value = 'disk';
  let last = null;
  const clear = () => { fillList($('sumList'), []); last = null; $('sumCopy').disabled = true; };
  clear();
  $('sumWhat').onchange = clear;
  $('sumCompute').onclick = async () => {
    const what = $('sumWhat').value;
    $('sumCompute').disabled = true;
    setStatus('Computing checksums…');
    try {
      last = await invoke('checksum', { what, path: what === 'file' ? file.path : null });
      fillList($('sumList'), [
        ['Bytes', fmtBytes(last.bytes)], ['CRC-32', last.crc32], ['Adler-32', last.adler32],
        ['MD5', last.md5], ['SHA-1', last.sha1], ['SHA-256', last.sha256],
      ]);
      $('sumCopy').disabled = false;
      setStatus('Checksums computed');
    } catch (e) {
      setStatus(e && e.message ? e.message : String(e), 'error');
    } finally {
      $('sumCompute').disabled = false;
    }
  };
  $('sumCopy').onclick = async () => {
    if (!last) return;
    const text = ['bytes', 'crc32', 'adler32', 'md5', 'sha1', 'sha256'].map((k) => `${k} ${last[k]}`).join('\n');
    try { await navigator.clipboard.writeText(`${text}\n`); setStatus('Checksums copied'); } catch { setStatus('Could not use the clipboard', 'error'); }
  };
  await showDialog($('sumDlg'));
}

async function actionListing() {
  const where = state.dir ? `-${baseName(state.dir)}` : '';
  const dest = await dialog.save({
    title: 'Export file list', defaultPath: `${stem(state.image.name)}${where}.html`,
    filters: [{ name: 'Web page', extensions: ['html'] }, { name: 'Text', extensions: ['txt'] }, { name: 'CSV (spreadsheet)', extensions: ['csv'] }],
  });
  if (!dest) return;
  const ext = dest.split('.').pop().toLowerCase();
  const format = ext === 'csv' ? 'csv' : ext === 'txt' ? 'text' : 'html';
  const n = await invoke('export_listing', { dest, format, dir: state.dir });
  setStatus(`Listed ${n} item(s) in ${dest}`);
}

async function actionTruncate() {
  const yes = await dialog.ask(
    'Cut the unused space off the end of this image file? Every file stays where it is; the image just stops after the last used cluster. '
    + 'It still opens here and grows again when files are added, but some tools and emulators expect the full size.',
    { title: 'Truncate unused space', kind: 'warning' },
  );
  if (!yes) return;
  const r = await invoke('truncate');
  await refreshInfo();
  render();
  setStatus(r.new_size === r.old_size ? 'Nothing to cut: the last cluster is in use'
    : `Truncated ${human(r.old_size)} → ${human(r.new_size)}`);
}

async function actionIsoReplace() {
  const items = selectedEntries();
  if (items.length !== 1 || items[0].is_dir) {
    await dialog.message('Select one file on the CD image first.', { title: 'Replace file' });
    return;
  }
  const target = items[0];
  const src = await dialog.open({ title: `Replace ${target.name} with`, multiple: false });
  if (!src) return;
  const yes = await dialog.ask(
    `Overwrite ${target.name} on ${state.image.name} with ${baseName(src)}? The new file must fit in the space the old one takes (${fmtBytes(Math.ceil(target.size / 2048) * 2048)} bytes). This changes the image file directly.`,
    { title: 'Replace file', kind: 'warning' },
  );
  if (!yes) return;
  await invoke('iso_replace', { path: target.path, src });
  await refresh(true);
  setStatus(`Replaced ${target.name}`);
}

// Double-click or Enter on a file: copy it out to a temporary folder and open it.
async function openEntry(e) {
  setStatus(`Opening ${e.name}…`);
  await invoke('open_entry', { path: e.path });
  setStatus(`Opened a copy of ${e.name} (changes are not saved into the image)`);
}

// Drag rows out to Explorer / the desktop: files are copied out first, then a native drag starts.
const drag = { start: null, active: false };

async function dragOut() {
  const paths = [...state.selected];
  setStatus(`Preparing ${paths.length} item(s) to drag…`);
  drag.active = true;
  try {
    const dropped = await invoke('drag_out', { paths });
    setStatus(dropped ? `Dragged out ${paths.length} item(s)` : 'Drag cancelled');
  } finally {
    drag.active = false;
    $('dropzone').hidden = true;
  }
}

function enter(dir) {
  state.dir = dir;
  return refresh();
}

// ---------- drives ----------

// Progress events from long drive operations show in the status bar.
window.__TAURI__.event.listen('progress', ({ payload: p }) => {
  const verb = { reading: 'Reading', writing: 'Writing', verifying: 'Verifying' }[p.phase] || p.phase;
  const pct = p.total ? Math.floor((p.done * 100) / p.total) : 0;
  setStatus(`${verb} ${pct}% (${human(p.done)} of ${human(p.total)})`);
});

function driveLine(d) {
  const bits = [d.bus, d.media ? human(d.size) : 'no medium'];
  if (d.volumes.length) bits.push(d.volumes.join(' '));
  if (d.system) bits.push('system disk, never written');
  else if (!d.removable) bits.push('fixed disk');
  return bits.join(' · ');
}

async function fillDrives() {
  const list = $('driveList');
  list.replaceChildren();
  const drives = await invoke('drives');
  for (const d of drives) {
    const label = document.createElement('label');
    const input = document.createElement('input');
    input.type = 'radio';
    input.name = 'drive';
    input.value = d.path;
    input.disabled = !d.media;
    input.onchange = syncDriveButtons;
    input._drive = d;
    const title = document.createElement('span');
    title.textContent = `${d.name}${d.model ? ` — ${d.model}` : ''}`;
    const sub = document.createElement('span');
    sub.className = 'sub';
    sub.textContent = `${driveLine(d)} · ${d.path}`;
    label.append(input, title, sub);
    list.append(label);
  }
  // Preselect the first removable drive with a medium.
  const first = [...list.querySelectorAll('input')].find((i) => !i.disabled && i._drive.removable && !i._drive.system);
  if (first) first.checked = true;
  syncDriveButtons();
}

function selectedDrive() {
  return document.querySelector('#driveList input:checked')?._drive ?? null;
}

function syncDriveButtons() {
  const d = selectedDrive();
  const fixed = !!d && !d.removable && !d.system;
  $('driveFixedWrap').hidden = !fixed;
  if (!fixed) $('driveFixed').checked = false;
  $('driveOpen').disabled = !d;
  $('driveRead').disabled = !d;
  $('driveWrite').disabled = !d || d.system || (fixed && !$('driveFixed').checked);
}

async function actionDrives() {
  $('driveFixed').onchange = syncDriveButtons;
  $('driveRefresh').onclick = () => fillDrives().catch(showError);
  await fillDrives();
  const what = await showDialog($('drivesDlg'));
  const d = selectedDrive();
  if (!d || !['open', 'read', 'write'].includes(what)) return;
  if (what === 'open') {
    await openImage(d.path);
    return;
  }
  if (what === 'read') {
    $('saveAsTitle').textContent = `Read ${d.name} to an image`;
    const sync = () => { $('saveDynamic').disabled = !DYNAMIC.includes($('saveContainer').value); };
    $('saveContainer').onchange = sync;
    sync();
    if (await showDialog($('saveAsDlg')) !== 'ok') return;
    const container = $('saveContainer').value;
    const name = d.name.replace(/[^A-Za-z0-9 _-]+/g, '').trim() || 'drive';
    const dest = await dialog.save({ title: `Read ${d.name} to`, defaultPath: `${name}.${CONTAINER_EXT[container]}`, filters: filterFor(container) });
    if (!dest) return;
    setStatus(`Reading ${d.name}…`);
    const r = await invoke('read_drive', { drive: d.path, dest, container, dynamic: !$('saveDynamic').disabled && $('saveDynamic').checked });
    setStatus(`Read ${d.name} into ${baseName(dest)} (${human(r.bytes)}, ${human(r.stored)} of data)`);
    if (await dialog.ask(`Read ${d.name} into ${baseName(dest)}. Open the image now?`, { title: 'Read drive', kind: 'info' })) await openImage(dest);
    return;
  }
  const image = await dialog.open({ title: `Image to write to ${d.name}`, multiple: false, directory: false, filters: IMAGE_FILTERS, defaultPath: state.image?.path });
  if (!image) return;
  const sure = await dialog.ask(
    `Everything on ${d.name}${d.model ? ` (${d.model})` : ''}, ${driveLine(d)}, will be replaced with ${baseName(image)}.\n\nThis cannot be undone.`,
    { title: 'Write image to drive', kind: 'warning', okLabel: 'Write', cancelLabel: 'Cancel' },
  );
  if (!sure) return;
  setStatus(`Writing ${baseName(image)} to ${d.name}…`);
  const r = await invoke('write_drive', { image, drive: d.path, verify: $('driveVerify').checked, allowFixed: $('driveFixed').checked });
  if (state.image && state.image.path.toLowerCase() === d.path.toLowerCase()) await openImage(d.path);
  setStatus(`Wrote ${human(r.bytes)} to ${d.name}${r.verified ? ', verified' : ''}`);
}

// ---------- rendering ----------

function updateButtons() {
  const has = !!state.image;
  document.querySelectorAll('[data-needs-image]').forEach((b) => { b.disabled = !has || state.busy; });
  document.querySelectorAll('[data-needs-writable]').forEach((b) => { b.disabled = !writable() || state.busy; });
  document.querySelectorAll('[data-needs-selection]').forEach((b) => { b.disabled ||= state.selected.size === 0; });
  $('btnRename').disabled ||= state.selected.size !== 1;
  for (const id of ['btnNew', 'btnOpen', 'btnDrives', 'btnIso']) $(id).disabled = state.busy;
  const iso = has && state.image.info.iso;
  $('btnBootImage').hidden = !iso;
  for (const id of ['btnLabel', 'btnBoot', 'btnDefrag', 'btnFormat']) $(id).hidden = !!iso;
  $('btnFormat').disabled ||= !has || state.image.info.layout.kind !== 'BareVolume';
  const sel = selectedEntries();
  $('mnuIsoReplace').hidden = !iso;
  $('mnuIsoReplace').disabled = !(sel.length === 1 && !sel[0].is_dir);
  $('mnuTruncate').hidden = !!iso;
  $('mnuDiff').disabled = !(has && state.image.info.container.kind === 'vhd');
  $('mnuTruncate').disabled = !(writable() && state.image.info.container.kind === 'raw'
    && state.image.info.layout.kind === 'BareVolume');
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
      tr.ondblclick = () => run(() => (e.is_dir ? enter(e.path) : openEntry(e)));
      tr.onmousedown = (ev) => {
        if (ev.button === 0 && !ev.shiftKey && !ev.ctrlKey) drag.start = { x: ev.clientX, y: ev.clientY, idx };
      };
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
$('btnDrives').onclick = () => run(actionDrives);
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
$('btnProps').onclick = () => run(actionProps);

// Tools menu: a popover placed under its button; each item closes it first.
$('toolsMenu').addEventListener('beforetoggle', (ev) => {
  if (ev.newState !== 'open') return;
  const r = $('btnTools').getBoundingClientRect();
  const m = $('toolsMenu');
  m.style.top = `${r.bottom + 4}px`;
  m.style.left = `${Math.max(8, Math.min(r.left, window.innerWidth - 240))}px`;
});
const menuItem = (id, fn) => { $(id).onclick = () => { $('toolsMenu').hidePopover(); run(fn); }; };
menuItem('mnuChecksum', actionChecksum);
menuItem('mnuListing', actionListing);
menuItem('mnuTruncate', actionTruncate);
menuItem('mnuIsoReplace', actionIsoReplace);
menuItem('mnuDiff', actionDiff);

// A press on a row that moves a few pixels starts dragging the selection out of the window.
document.addEventListener('mousemove', (ev) => {
  const s = drag.start;
  if (!s || drag.active || state.busy) return;
  if (!(ev.buttons & 1)) { drag.start = null; return; }
  if (Math.hypot(ev.clientX - s.x, ev.clientY - s.y) < 6) return;
  drag.start = null;
  const path = state.entries[s.idx]?.path;
  if (!path) return;
  if (!state.selected.has(path)) {
    state.selected.clear();
    state.selected.add(path);
    state.anchor = s.idx;
    render();
  }
  run(dragOut);
});
document.addEventListener('mouseup', () => { drag.start = null; });
$('partSel').onchange = () => run(() => openImage(state.image.path, Number($('partSel').value)));
document.querySelectorAll('input[name="newKind"]').forEach((r) => { r.onchange = syncNewDialog; });
$('newContainer').onchange = syncNewDialog;
$('newFs').onchange = syncNewDialog;

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
  else if (k === 'Enter' && ev.altKey && state.selected.size) run(actionProps);
  else if (k === 'Enter' && state.selected.size === 1) {
    const e = state.entries.find((x) => state.selected.has(x.path));
    if (e) run(() => (e.is_dir ? enter(e.path) : openEntry(e)));
  }
});

// Files dropped from Explorer: open an image, or inject into the current folder.
getCurrentWebview().onDragDropEvent((event) => {
  // Our own drag-out passing over (or dropped back on) the window is not an inject.
  if (drag.active) return;
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
