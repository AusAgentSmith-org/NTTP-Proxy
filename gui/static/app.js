// ─── Login / Logout ──────────────────────────────────────────────────────

const loginScreen = document.getElementById('login-screen');
const appMain     = document.getElementById('app-main');
const mainNav     = document.getElementById('main-nav');
const userBar     = document.getElementById('user-bar');
const userName    = document.getElementById('user-name');
const loginForm   = document.getElementById('login-form');
const loginUser   = document.getElementById('login-user');
const loginPass   = document.getElementById('login-pass');
const loginError  = document.getElementById('login-error');

function showLogin() {
  loginScreen.classList.remove('hidden');
  appMain.classList.add('hidden');
  mainNav.classList.add('hidden');
  userBar.classList.add('hidden');
}

function showApp(session) {
  loginScreen.classList.add('hidden');
  appMain.classList.remove('hidden');
  mainNav.classList.remove('hidden');
  userBar.classList.remove('hidden');
  userName.textContent = `${session.username} · ${session.max_connections} conns`;
  refreshQueue();
}

async function checkSession() {
  const r = await fetch('/api/me');
  if (r.status === 200) {
    showApp(await r.json());
  } else {
    showLogin();
  }
}

loginForm.addEventListener('submit', async (e) => {
  e.preventDefault();
  loginError.textContent = '';
  try {
    const r = await fetch('/api/login', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        username: loginUser.value.trim(),
        password: loginPass.value,
      }),
    });
    if (!r.ok) throw new Error(await r.text());
    const session = await r.json();
    loginPass.value = '';
    showApp(session);
  } catch (err) {
    loginError.textContent = err.message || 'Login failed';
  }
});

document.getElementById('logout-btn').addEventListener('click', async () => {
  await fetch('/api/logout', { method: 'POST' });
  showLogin();
});

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
      <tr data-job-id="${escapeHtml(j.id)}">
        <td class="name">${escapeHtml(j.name)}</td>
        <td><span class="status-pill ${cls}">${escapeHtml(j.status)}</span></td>
        <td style="min-width: 220px;">
          <div class="bar">
            <div class="fill ${failCls}" style="width: ${pct.toFixed(1)}%"></div>
            <div class="label">${pct.toFixed(1)}% · ${fmtBytes(j.downloaded_bytes)} / ${fmtBytes(j.total_bytes)}</div>
          </div>
        </td>
        <td class="num">${j.articles_downloaded} / ${j.article_count}${j.articles_failed > 0 ? ` (${j.articles_failed} failed)` : ''}</td>
        <td class="actions">
          <button class="icon-btn cancel" title="Cancel download" data-action="cancel">✕</button>
        </td>
      </tr>
    `;
  }).join('');
}

// Event delegation for cancel buttons
document.querySelector('#active-table tbody').addEventListener('click', async (e) => {
  const btn = e.target.closest('button[data-action="cancel"]');
  if (!btn) return;
  const row = btn.closest('tr');
  const id = row.dataset.jobId;
  if (!id) return;
  btn.disabled = true;
  btn.textContent = '…';
  try {
    const r = await fetch(`/api/jobs/${encodeURIComponent(id)}`, { method: 'DELETE' });
    if (!r.ok) throw new Error(await r.text());
    refreshQueue();
  } catch (err) {
    console.error('cancel failed', err);
    btn.disabled = false;
    btn.textContent = '✕';
  }
});

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

// Poll the queue continuously; if we get 401 the interval handler will
// flip back to the login screen automatically.
setInterval(async () => {
  if (loginScreen.classList.contains('hidden')) {
    refreshQueue();
  }
}, 2000);

// React to a 401 anywhere by showing the login screen.
const _origFetch = window.fetch;
window.fetch = async (...args) => {
  const r = await _origFetch(...args);
  if (r.status === 401 && !args[0].toString().includes('/api/login')) {
    showLogin();
  }
  return r;
};

checkSession();

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

// ─── Search ──────────────────────────────────────────────────────────────
const searchForm  = document.getElementById('search-form');
const searchInput = document.getElementById('search-q');
const searchTable = document.getElementById('search-table');
const searchBody  = searchTable.querySelector('tbody');
const searchStatus = document.getElementById('search-status');

const fmtAge = (epochSecs) => {
  if (!epochSecs) return '—';
  const ageSecs = Math.floor(Date.now() / 1000) - epochSecs;
  if (ageSecs < 0) return 'just now';
  const d = Math.floor(ageSecs / 86400);
  if (d > 365) return `${(d/365).toFixed(1)}y`;
  if (d > 30)  return `${Math.floor(d/30)}mo`;
  if (d > 0)   return `${d}d`;
  const h = Math.floor(ageSecs / 3600);
  if (h > 0)   return `${h}h`;
  const m = Math.floor(ageSecs / 60);
  return `${m}m`;
};

searchForm.addEventListener('submit', async (e) => {
  e.preventDefault();
  const q = searchInput.value.trim();
  if (!q) return;
  searchStatus.textContent = 'Searching…';
  searchTable.classList.add('hidden');
  try {
    const r = await fetch(`/api/search?q=${encodeURIComponent(q)}&limit=50`);
    if (!r.ok) throw new Error(await r.text());
    const data = await r.json();
    if (data.error) {
      searchStatus.textContent = data.error;
      return;
    }
    const releases = data.releases || [];
    if (!releases.length) {
      searchStatus.textContent = 'No results.';
      return;
    }
    searchStatus.textContent = `${releases.length} result(s).`;
    searchTable.classList.remove('hidden');
    searchBody.innerHTML = releases.map(r => `
      <tr data-rel-id="${r.id}">
        <td class="name" title="${escapeHtml(r.name)}">${escapeHtml(truncate(r.name, 80))}</td>
        <td class="num">${escapeHtml(r.newsgroup || '—')}</td>
        <td class="num">${escapeHtml(fmtAge(r.posted_at))}</td>
        <td class="num">${fmtBytes(r.total_bytes)}</td>
        <td class="num">${r.total_files ?? '—'}</td>
        <td class="actions">
          <button class="icon-btn grab" title="Download" data-action="grab">↓</button>
        </td>
      </tr>
    `).join('');
  } catch (err) {
    console.error('search failed', err);
    searchStatus.textContent = `Error: ${err.message}`;
  }
});

function truncate(s, n) {
  if (!s || s.length <= n) return s || '';
  return s.slice(0, n - 1) + '…';
}

searchBody.addEventListener('click', async (e) => {
  const btn = e.target.closest('button[data-action="grab"]');
  if (!btn) return;
  const row = btn.closest('tr');
  const id = row.dataset.relId;
  if (!id) return;
  btn.disabled = true;
  btn.textContent = '…';
  try {
    const r = await fetch(`/api/grab/${encodeURIComponent(id)}`, { method: 'POST' });
    if (!r.ok) throw new Error(await r.text());
    btn.textContent = '✓';
    btn.title = 'Added to queue';
    refreshQueue();
  } catch (err) {
    console.error('grab failed', err);
    btn.disabled = false;
    btn.textContent = '↓';
    alert(`Grab failed: ${err.message}`);
  }
});
