// ─── Tabs ────────────────────────────────────────────────────────────────
document.querySelectorAll('.tab').forEach(t => {
  t.addEventListener('click', () => {
    document.querySelectorAll('.tab').forEach(x => x.classList.remove('active'));
    document.querySelectorAll('.tab-panel').forEach(x => x.classList.remove('active'));
    t.classList.add('active');
    document.getElementById(`tab-${t.dataset.tab}`).classList.add('active');
  });
});

// ─── Helpers ─────────────────────────────────────────────────────────────
const fmtBytes = (n) => {
  if (!n) return '0';
  const u = ['B','KB','MB','GB','TB'];
  let i = 0; let v = n;
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
  return `${v.toFixed(v < 10 ? 1 : 0)} ${u[i]}`;
};

const pillClass = (status) => {
  const s = status.toLowerCase();
  if (s.includes('download')) return 'downloading';
  if (s.includes('complet')) return 'completed';
  if (s.includes('fail'))    return 'failed';
  return '';
};

// ─── Queue polling ───────────────────────────────────────────────────────
async function refreshQueue() {
  try {
    const r = await fetch('/api/queue');
    const data = await r.json();
    renderActive(data.active);
    renderHistory(data.history);
  } catch (e) {
    console.error('queue refresh failed', e);
  }
}

function renderActive(jobs) {
  const body = document.querySelector('#active-table tbody');
  const empty = document.getElementById('active-empty');
  const table = document.getElementById('active-table');
  if (!jobs.length) {
    body.innerHTML = '';
    empty.classList.remove('hidden');
    table.classList.add('hidden');
    return;
  }
  empty.classList.add('hidden');
  table.classList.remove('hidden');

  body.innerHTML = jobs.map(j => {
    const pct = Math.min(100, Math.max(0, j.percent || 0));
    const cls = pillClass(j.status);
    const failCls = j.articles_failed > 0 ? 'failed' : (pct >= 99.9 ? 'complete' : '');
    return `
      <tr>
        <td class="name">${escapeHtml(j.name)}</td>
        <td><span class="status-pill ${cls}">${escapeHtml(j.status)}</span></td>
        <td style="min-width: 220px;">
          <div class="bar">
            <div class="fill ${failCls}" style="width: ${pct.toFixed(1)}%"></div>
            <div class="label">${pct.toFixed(1)}% · ${fmtBytes(j.downloaded_bytes)} / ${fmtBytes(j.total_bytes)}</div>
          </div>
        </td>
        <td class="num">${j.articles_downloaded} / ${j.article_count}${j.articles_failed > 0 ? ` (${j.articles_failed} failed)` : ''}</td>
      </tr>
    `;
  }).join('');
}

function renderHistory(jobs) {
  const body = document.querySelector('#history-table tbody');
  const empty = document.getElementById('history-empty');
  const table = document.getElementById('history-table');
  if (!jobs.length) {
    body.innerHTML = '';
    empty.classList.remove('hidden');
    table.classList.add('hidden');
    return;
  }
  empty.classList.add('hidden');
  table.classList.remove('hidden');

  body.innerHTML = jobs.map(j => `
    <tr>
      <td class="name">${escapeHtml(j.name)}</td>
      <td><span class="status-pill ${pillClass(j.status)}">${escapeHtml(j.status)}</span></td>
      <td class="num">${fmtBytes(j.total_bytes)}</td>
    </tr>
  `).join('');
}

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, c => ({
    '&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'
  })[c]);
}

setInterval(refreshQueue, 2000);
refreshQueue();

// ─── Upload ──────────────────────────────────────────────────────────────
const dz = document.getElementById('dropzone');
const input = document.getElementById('file-input');
const result = document.getElementById('upload-result');

['dragover', 'dragenter'].forEach(ev =>
  dz.addEventListener(ev, e => { e.preventDefault(); dz.classList.add('over'); }));
['dragleave', 'drop'].forEach(ev =>
  dz.addEventListener(ev, e => { e.preventDefault(); dz.classList.remove('over'); }));

dz.addEventListener('drop', e => upload(e.dataTransfer.files));
input.addEventListener('change', e => upload(e.target.files));

async function upload(fileList) {
  if (!fileList || !fileList.length) return;
  const form = new FormData();
  for (const f of fileList) form.append('file', f, f.name);

  const tmp = document.createElement('div');
  tmp.className = 'entry';
  tmp.textContent = `Uploading ${fileList.length} file(s)…`;
  result.prepend(tmp);

  try {
    const r = await fetch('/api/upload', { method: 'POST', body: form });
    if (!r.ok) throw new Error(await r.text());
    const data = await r.json();
    tmp.className = 'entry ok';
    tmp.textContent = `Added ${data.added} job(s)` + (data.errors?.length ? ` · ${data.errors.length} error(s)` : '');
    if (data.errors?.length) {
      data.errors.forEach(err => {
        const e = document.createElement('div');
        e.className = 'entry err';
        e.textContent = err;
        result.prepend(e);
      });
    }
    refreshQueue();
  } catch (e) {
    tmp.className = 'entry err';
    tmp.textContent = `Upload failed: ${e.message}`;
  }
  input.value = '';
}
