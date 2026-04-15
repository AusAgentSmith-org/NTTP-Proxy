// ─── Auth (very basic — bearer token in localStorage) ───────────────────
const TOKEN_KEY = 'nzb_admin_token';

function getToken()  { return localStorage.getItem(TOKEN_KEY) || ''; }
function setToken(t) { localStorage.setItem(TOKEN_KEY, t); }
function clearToken(){ localStorage.removeItem(TOKEN_KEY); }

function showSetup() {
  document.getElementById('setup').classList.remove('hidden');
  document.getElementById('app').classList.add('hidden');
  document.getElementById('auth-state').textContent = 'not authorised';
}
function showApp() {
  document.getElementById('setup').classList.add('hidden');
  document.getElementById('app').classList.remove('hidden');
  document.getElementById('auth-state').textContent = 'authorised';
}

document.getElementById('auth-form').addEventListener('submit', (e) => {
  e.preventDefault();
  const t = document.getElementById('admin-token').value.trim();
  if (!t) return;
  setToken(t);
  init();
});

document.getElementById('logout-btn').addEventListener('click', () => {
  clearToken();
  showSetup();
});

// ─── API helpers ────────────────────────────────────────────────────────
async function api(path, opts = {}) {
  const r = await fetch(path, {
    ...opts,
    headers: {
      ...(opts.headers || {}),
      'Authorization': `Bearer ${getToken()}`,
      'Content-Type': 'application/json',
    },
  });
  if (r.status === 401) {
    clearToken();
    showSetup();
    throw new Error('unauthorised');
  }
  if (!r.ok) throw new Error(`${r.status}: ${await r.text()}`);
  return r.json();
}

// ─── UI helpers ─────────────────────────────────────────────────────────
const fmtBytes = (n) => {
  if (!n) return '0';
  const u = ['B','KB','MB','GB','TB'];
  let i = 0; let v = n;
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
  return `${v.toFixed(v < 10 ? 1 : 0)} ${u[i]}`;
};

const fmtAgo = (iso) => {
  if (!iso) return '—';
  const ageSecs = Math.floor((Date.now() - new Date(iso).getTime()) / 1000);
  if (ageSecs < 0) return 'just now';
  if (ageSecs < 5) return 'now';
  if (ageSecs < 60) return `${ageSecs}s ago`;
  const m = Math.floor(ageSecs / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ago`;
  return `${Math.floor(h / 24)}d ago`;
};

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, c => ({
    '&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'
  })[c]);
}

// ─── Render users ──────────────────────────────────────────────────────
async function refreshUsers() {
  try {
    const data = await api('/api/admin/users');
    const body = document.querySelector('#users-table tbody');
    if (!data.users.length) {
      body.innerHTML = `<tr><td colspan="7" class="muted" style="text-align:center;padding:2rem;">No users yet.</td></tr>`;
      return;
    }
    body.innerHTML = data.users.map(u => `
      <tr data-username="${escapeHtml(u.username)}">
        <td class="name">${escapeHtml(u.username)}</td>
        <td>
          <span class="status-pill ${u.locked ? 'failed' : 'completed'}">
            ${u.locked ? 'LOCKED' : 'active'}
          </span>
        </td>
        <td class="num">
          <span class="${u.active_sessions > 0 ? 'live' : ''}">${u.active_sessions}</span>
          / ${u.max_connections}
        </td>
        <td class="num">${fmtBytes(u.bytes_total)}</td>
        <td class="num">${u.total_sessions}</td>
        <td class="num">${fmtAgo(u.last_seen)}</td>
        <td class="actions">
          <button class="icon-btn lock"   data-action="${u.locked ? 'unlock' : 'lock'}"
                  title="${u.locked ? 'Unlock' : 'Lock'}">${u.locked ? '🔓' : '🔒'}</button>
          <button class="icon-btn cancel" data-action="delete" title="Delete">✕</button>
        </td>
      </tr>
    `).join('');
  } catch (e) {
    console.error('refresh failed', e);
  }
}

document.getElementById('refresh-btn').addEventListener('click', refreshUsers);

document.querySelector('#users-table tbody').addEventListener('click', async (e) => {
  const btn = e.target.closest('button[data-action]');
  if (!btn) return;
  const row = btn.closest('tr');
  const u = row.dataset.username;
  const action = btn.dataset.action;
  btn.disabled = true;
  try {
    if (action === 'delete') {
      if (!confirm(`Delete user "${u}"?`)) { btn.disabled = false; return; }
      await api(`/api/admin/users/${encodeURIComponent(u)}`, { method: 'DELETE' });
    } else {
      await api(`/api/admin/users/${encodeURIComponent(u)}/lock`, {
        method: 'PUT',
        body: JSON.stringify({ locked: action === 'lock' }),
      });
    }
    refreshUsers();
  } catch (err) {
    alert(`Failed: ${err.message}`);
    btn.disabled = false;
  }
});

// ─── Create user ───────────────────────────────────────────────────────
document.getElementById('create-form').addEventListener('submit', async (e) => {
  e.preventDefault();
  const username = document.getElementById('new-username').value.trim();
  const password = document.getElementById('new-password').value;
  const max_connections = parseInt(document.getElementById('new-max').value, 10);
  const result = document.getElementById('create-result');
  try {
    await api('/api/admin/users', {
      method: 'POST',
      body: JSON.stringify({ username, password, max_connections }),
    });
    result.textContent = `Created "${username}".`;
    result.style.color = 'var(--ok)';
    document.getElementById('create-form').reset();
    document.getElementById('new-max').value = 8;
    refreshUsers();
  } catch (err) {
    result.textContent = err.message;
    result.style.color = 'var(--err)';
  }
});

// ─── Init ──────────────────────────────────────────────────────────────
async function init() {
  if (!getToken()) { showSetup(); return; }
  try {
    await api('/api/admin/users');
    showApp();
    refreshUsers();
    setInterval(refreshUsers, 2000);
  } catch (e) {
    showSetup();
  }
}

init();
