// File library: folders, upload (button or drag & drop), metadata, replace, delete.

import { api, assetUrl, thumbUrl } from '../api.js';
import { add, button, confirmDialog, fail, h, inputEl, labeled, modal, replace, selectEl, toast } from '../dom.js';
import { clearOptionCache } from '../form.js';
import { formatBytes, truncate } from '../util.js';

const PAGE_SIZE = 48;

function thumb(f) {
  if (f.type && !String(f.type).startsWith('image/')) return h('div', { class: 'file-chip big' }, (f.type.split('/')[1] || 'file').toUpperCase());
  return h('img', {
    class: 'thumb',
    src: thumbUrl(f.id, f.type, 320),
    alt: f.filename_download || '',
    loading: 'lazy',
    onError: (e) => e.target.replaceWith(h('div', { class: 'file-chip big' }, 'FILE')),
  });
}

async function uploadFiles(files, folder) {
  for (const file of files) {
    const fd = new FormData();
    if (folder) fd.append('folder', folder); // text parts must precede the file part
    fd.append('file', file);
    await api.upload('/files', fd);
  }
  clearOptionCache();
}

export async function filesView(root) {
  const state = { page: 1, folder: '', search: '' };
  const folders = (await api.get('/folders', { fields: 'id,name,parent', limit: 500, sort: 'name' })).data;
  const grid = h('div', { class: 'file-grid big' });
  const pager = h('div', { class: 'pager' });
  const folderOptions = [{ value: '', label: 'All files' }, { value: '__root', label: 'No folder' }, ...folders.map((f) => ({ value: f.id, label: f.name }))];

  async function load(page = state.page) {
    state.page = page;
    let filter;
    if (state.folder === '__root') filter = { folder: { _null: true } };
    else if (state.folder) filter = { folder: { _eq: state.folder } };
    const r = await api.get('/files', {
      fields: 'id,title,filename_download,type,filesize,folder,description,uploaded_on',
      filter,
      search: state.search,
      sort: '-uploaded_on',
      limit: PAGE_SIZE,
      page,
      meta: 'filter_count',
    });
    replace(grid, 
      ...(r.data.length
        ? r.data.map((f) =>
            h(
              'button',
              { type: 'button', class: 'file-card', onClick: () => fileDialog(f, folders, () => load().catch(fail)) },
              thumb(f),
              h('span', { class: 'file-name' }, truncate(f.title || f.filename_download, 30)),
              h('span', { class: 'muted small' }, formatBytes(f.filesize)),
            ),
          )
        : [h('p', { class: 'muted' }, 'No files. Upload some or drop them here.')]),
    );
    const total = r.meta?.filter_count ?? r.data.length;
    const pages = Math.max(1, Math.ceil(total / PAGE_SIZE));
    replace(pager, 
      button('‹', () => state.page > 1 && load(state.page - 1).catch(fail), 'sm'),
      h('span', {}, ` ${state.page} / ${pages} · ${total} files `),
      button('›', () => state.page < pages && load(state.page + 1).catch(fail), 'sm'),
    );
  }

  const picker = h('input', {
    type: 'file',
    multiple: true,
    style: { display: 'none' },
    onChange: async (e) => {
      try {
        await uploadFiles([...e.target.files], state.folder && state.folder !== '__root' ? state.folder : null);
        toast('Uploaded', 'ok');
        await load(1);
      } catch (err) {
        fail(err);
      }
      e.target.value = '';
    },
  });

  const zone = h(
    'div',
    {
      class: 'dropzone',
      onDragover: (e) => {
        e.preventDefault();
        zone.className = 'dropzone over';
      },
      onDragleave: () => (zone.className = 'dropzone'),
      onDrop: async (e) => {
        e.preventDefault();
        zone.className = 'dropzone';
        try {
          await uploadFiles([...e.dataTransfer.files], state.folder && state.folder !== '__root' ? state.folder : null);
          toast('Uploaded', 'ok');
          await load(1);
        } catch (err) {
          fail(err);
        }
      },
    },
    grid,
  );

  add(root, 
    h(
      'div',
      { class: 'page-head' },
      h('h1', {}, 'Files'),
      h('span', { class: 'spacer' }),
      selectEl(folderOptions, state.folder, (v) => {
        state.folder = v;
        load(1).catch(fail);
      }),
      button('+ Folder', () => newFolder(folders, state.folder)),
      h('input', {
        type: 'search',
        placeholder: 'Search…',
        onKeydown: (e) => {
          if (e.key === 'Enter') {
            state.search = e.target.value;
            load(1).catch(fail);
          }
        },
      }),
      button('Upload', () => picker.click(), 'primary'),
      picker,
    ),
    zone,
    pager,
  );
  await load(1);
}

function newFolder(folders, current) {
  const draft = { name: '', parent: current && current !== '__root' ? current : null };
  modal({
    title: 'New folder',
    body: h(
      'div',
      {},
      labeled('Name', inputEl('', (v) => (draft.name = v.trim()))),
      labeled('Parent', selectEl([{ value: '', label: '— none —' }, ...folders.map((f) => ({ value: f.id, label: f.name }))], draft.parent || '', (v) => (draft.parent = v || null))),
    ),
    buttons: [
      { label: 'Cancel' },
      {
        label: 'Create',
        class: 'primary',
        onClick: async () => {
          if (!draft.name) throw new Error('Name is required');
          await api.post('/folders', draft);
          clearOptionCache();
          location.reload();
        },
      },
    ],
  });
}

function fileDialog(file, folders, onChange) {
  const draft = { title: file.title || '', description: file.description || '', filename_download: file.filename_download || '', folder: file.folder || null };
  const replace = h('input', {
    type: 'file',
    style: { display: 'none' },
    onChange: async (e) => {
      const f = e.target.files[0];
      if (!f) return;
      try {
        const fd = new FormData();
        fd.append('file', f);
        await api.upload('/files', fd, { replace: file.id });
        toast('Binary replaced', 'ok');
        dlg.close();
        onChange();
      } catch (err) {
        fail(err);
      }
    },
  });
  const dlg = modal({
    title: file.filename_download || file.id,
    wide: true,
    body: h(
      'div',
      { class: 'two-col' },
      h('div', { class: 'preview' }, thumb(file), h('p', { class: 'muted small' }, `${file.type || ''} · ${formatBytes(file.filesize)}`), h('a', { href: assetUrl(file.id, '&download=1'), class: 'button-link' }, 'Download'), button('Replace binary', () => replace.click(), 'sm'), replace),
      h(
        'div',
        {},
        labeled('Title', inputEl(draft.title, (v) => (draft.title = v))),
        labeled('File name', inputEl(draft.filename_download, (v) => (draft.filename_download = v))),
        labeled('Description', h('textarea', { rows: 3, onInput: (e) => (draft.description = e.target.value) }, draft.description)),
        labeled('Folder', selectEl([{ value: '', label: '— none —' }, ...folders.map((f) => ({ value: f.id, label: f.name }))], draft.folder || '', (v) => (draft.folder = v || null))),
        h('p', { class: 'muted small' }, `id ${file.id}`),
      ),
    ),
    buttons: [
      {
        label: 'Delete',
        class: 'danger',
        close: false,
        onClick: async () => {
          if (!(await confirmDialog('Delete this file? Items referencing it lose the link.', { label: 'Delete', danger: true }))) return;
          await api.del(`/files/${file.id}`);
          clearOptionCache();
          dlg.close();
          onChange();
        },
      },
      { label: 'Cancel' },
      {
        label: 'Save',
        class: 'primary',
        onClick: async () => {
          await api.patch(`/files/${file.id}`, draft);
          clearOptionCache();
          onChange();
        },
      },
    ],
  });
}
