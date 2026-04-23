// Settings page: directory management + directory picker modal.
//
// No framework — only fetch + small DOM helpers. Data comes from /api/directories,
// /api/fs/list, etc. Percent-encoding is handled by encodeURIComponent; nothing is
// built via string concatenation into HTML.

(() => {
    const $ = (sel, root = document) => root.querySelector(sel);

    const addBtn = $('#btn-add-dir');
    const modal = $('#picker-modal');
    const pathInput = $('#picker-path');
    const entriesList = $('#picker-entries');
    const labelInput = $('#picker-label');
    const addSelectedBtn = $('#picker-add');
    const cancelBtn = $('#picker-cancel');
    const upBtn = $('#picker-up');
    const homeBtn = $('#picker-home');
    const goBtn = $('#picker-go');
    const status = $('#picker-status');

    let current = { path: '', parent: null, entries: [] };

    function openPicker() {
        modal.hidden = false;
        load(null).catch(err => setStatus(String(err)));
    }
    function closePicker() {
        modal.hidden = true;
    }
    function setStatus(msg) {
        status.textContent = msg || '';
    }

    async function load(path) {
        const url = path ? `/api/fs/list?path=${encodeURIComponent(path)}` : '/api/fs/list';
        const resp = await fetch(url);
        if (!resp.ok) {
            const body = await resp.json().catch(() => ({ error: 'unknown' }));
            setStatus(`Error: ${body.error}`);
            return;
        }
        current = await resp.json();
        pathInput.value = current.path;
        render();
        setStatus('');
    }

    function render() {
        entriesList.innerHTML = '';
        for (const e of current.entries) {
            const li = document.createElement('li');
            li.textContent = e.name + (e.is_dir ? '/' : '');
            if (!e.readable) {
                li.classList.add('disabled');
            }
            li.dataset.path = e.path;
            li.addEventListener('click', () => {
                if (e.readable) load(e.path);
            });
            entriesList.appendChild(li);
        }
    }

    async function addSelected() {
        const chosen = current.path;
        setStatus('Adding…');
        const body = { path: chosen };
        const lbl = labelInput.value.trim();
        if (lbl) body.label = lbl;
        const resp = await fetch('/api/directories', {
            method: 'POST',
            headers: { 'content-type': 'application/json' },
            body: JSON.stringify(body),
        });
        if (!resp.ok) {
            const err = await resp.json().catch(() => ({ error: 'unknown' }));
            setStatus(`Error: ${err.error}${err.message ? ' — ' + err.message : ''}`);
            return;
        }
        closePicker();
        window.location.reload();
    }

    if (addBtn) addBtn.addEventListener('click', openPicker);
    if (cancelBtn) cancelBtn.addEventListener('click', closePicker);
    if (addSelectedBtn) addSelectedBtn.addEventListener('click', addSelected);
    if (upBtn) upBtn.addEventListener('click', () => {
        if (current.parent) load(current.parent);
    });
    if (homeBtn) homeBtn.addEventListener('click', () => load(null));
    if (goBtn) goBtn.addEventListener('click', () => load(pathInput.value));
    if (pathInput) pathInput.addEventListener('keydown', ev => {
        if (ev.key === 'Enter') load(pathInput.value);
    });

    // Row-level actions: rename + remove + rescan.
    document.addEventListener('click', async ev => {
        const target = ev.target;
        if (!(target instanceof HTMLElement)) return;
        const row = target.closest('tr[data-dir-id]');

        if (target.id === 'btn-rescan-all') {
            await startScan(null);
            return;
        }

        if (!row) return;
        const id = row.dataset.dirId;

        if (target.classList.contains('btn-rescan')) {
            await startScan(id);
            return;
        }

        if (target.classList.contains('btn-remove')) {
            if (!confirm('Remove this directory?\n\n' +
                'Videos in this directory will be hidden from the UI. Watch history and\n' +
                'custom collection memberships are preserved. You can re-add this path later.')) {
                return;
            }
            const resp = await fetch(`/api/directories/${encodeURIComponent(id)}`, { method: 'DELETE' });
            if (!resp.ok) {
                alert('Remove failed');
                return;
            }
            window.location.reload();
        } else if (target.classList.contains('btn-rename')) {
            const cur = row.querySelector('.label-text').textContent;
            const next = prompt('New label:', cur);
            if (!next || next === cur) return;
            const resp = await fetch(`/api/directories/${encodeURIComponent(id)}`, {
                method: 'PATCH',
                headers: { 'content-type': 'application/json' },
                body: JSON.stringify({ label: next }),
            });
            if (!resp.ok) {
                alert('Rename failed');
                return;
            }
            window.location.reload();
        }
    });

    async function startScan(dirId) {
        const url = dirId ? `/api/scan?dir_id=${encodeURIComponent(dirId)}` : '/api/scan';
        const resp = await fetch(url, { method: 'POST' });
        if (!resp.ok) {
            alert('Scan failed to start');
            return;
        }
        pollScanStatus();
    }

    let pollTimer = null;
    function pollScanStatus() {
        const el = document.getElementById('scan-progress');
        if (!el) return;
        if (pollTimer) return;
        const tick = async () => {
            try {
                const resp = await fetch('/api/scan/status');
                if (!resp.ok) return;
                const s = await resp.json();
                if (s.phase === 'walking') {
                    el.textContent = `Scanning… files seen ${s.files_seen}, new ${s.new_videos}, changed ${s.changed_videos}, missing ${s.missing_videos}`;
                } else if (s.phase === 'done') {
                    el.textContent = `Scan complete. new ${s.new_videos}, changed ${s.changed_videos}, missing ${s.missing_videos}.`;
                    clearInterval(pollTimer);
                    pollTimer = null;
                    // Reload after a short delay so counts refresh.
                    setTimeout(() => window.location.reload(), 800);
                } else if (s.phase === 'failed') {
                    el.textContent = `Scan failed: ${s.error || 'unknown error'}`;
                    clearInterval(pollTimer);
                    pollTimer = null;
                } else {
                    el.textContent = '';
                }
            } catch (e) {
                // ignore transient errors
            }
        };
        pollTimer = setInterval(tick, 800);
        tick();
    }

    // On page load, if a scan is in progress, pick up polling.
    pollScanStatus();
})();
