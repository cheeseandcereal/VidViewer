// Settings page: directory management + directory picker modal + live status polling.
//
// No framework — only fetch + small DOM helpers. Data comes from /api/directories,
// /api/fs/list, and /api/scan/status. The page never reloads itself; all updates
// mutate the DOM in place so the user's scroll position, modal state, etc. are
// preserved while background work progresses.

(() => {
    const $ = (sel, root = document) => root.querySelector(sel);

    // --- Directory picker modal ---

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
    const pickerStatus = $('#picker-status');

    let picker = { path: '', parent: null, entries: [] };

    function openPicker() {
        modal.hidden = false;
        setPickerStatus('');
        labelInput.value = '';
        pickerLoad(null).catch(err => setPickerStatus(String(err)));
    }
    function closePicker() { modal.hidden = true; }
    function setPickerStatus(msg) { pickerStatus.textContent = msg || ''; }

    async function pickerLoad(path) {
        const url = path ? `/api/fs/list?path=${encodeURIComponent(path)}` : '/api/fs/list';
        const resp = await fetch(url);
        if (!resp.ok) {
            const body = await resp.json().catch(() => ({ error: 'unknown' }));
            setPickerStatus(`Error: ${body.error || 'unknown'}`);
            return;
        }
        picker = await resp.json();
        pathInput.value = picker.path;
        renderPickerEntries();
        setPickerStatus('');
    }

    function renderPickerEntries() {
        entriesList.innerHTML = '';
        for (const e of picker.entries) {
            const li = document.createElement('li');
            li.textContent = e.name + (e.is_dir ? '/' : '');
            if (!e.readable) li.classList.add('disabled');
            li.dataset.path = e.path;
            li.addEventListener('click', () => {
                if (e.readable) pickerLoad(e.path);
            });
            entriesList.appendChild(li);
        }
    }

    async function addCurrentDirectory() {
        const chosen = picker.path;
        if (!chosen) { setPickerStatus('No directory chosen'); return; }
        setPickerStatus('Adding…');
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
            setPickerStatus(`Error: ${err.error || 'unknown'}${err.message ? ' — ' + err.message : ''}`);
            return;
        }
        // Server auto-starts a scan for the new directory. We just close the modal
        // and let the polling loop pick up the new row and activity.
        closePicker();
        refreshDirectories();
        scheduleNextPoll(0); // immediate tick
    }

    if (addBtn) addBtn.addEventListener('click', openPicker);
    if (cancelBtn) cancelBtn.addEventListener('click', closePicker);
    if (addSelectedBtn) addSelectedBtn.addEventListener('click', addCurrentDirectory);
    if (upBtn) upBtn.addEventListener('click', () => {
        if (picker.parent) pickerLoad(picker.parent);
    });
    if (homeBtn) homeBtn.addEventListener('click', () => pickerLoad(null));
    if (goBtn) goBtn.addEventListener('click', () => pickerLoad(pathInput.value));
    if (pathInput) pathInput.addEventListener('keydown', ev => {
        if (ev.key === 'Enter') pickerLoad(pathInput.value);
    });

    // --- Row-level actions: rename, remove, rescan ---

    document.addEventListener('click', async ev => {
        const target = ev.target;
        if (!(target instanceof HTMLElement)) return;

        if (target.id === 'btn-rescan-all') {
            await startScan(null);
            return;
        }

        const row = target.closest('tr[data-dir-id]');
        if (!row) return;
        const id = row.dataset.dirId;

        if (target.classList.contains('btn-rescan')) {
            await startScan(id);
            return;
        }

        if (target.classList.contains('btn-remove')) {
            if (!confirm(
                'Remove this directory?\n\n' +
                'Videos in this directory will be hidden from the UI. Watch history and\n' +
                'custom collection memberships are preserved. You can re-add this path later.'
            )) return;
            const resp = await fetch(`/api/directories/${encodeURIComponent(id)}`, { method: 'DELETE' });
            if (!resp.ok) { alert('Remove failed'); return; }
            refreshDirectories();
        } else if (target.classList.contains('btn-rename')) {
            const labelEl = row.querySelector('.label-text');
            const cur = labelEl ? labelEl.textContent : '';
            const next = prompt('New label:', cur);
            if (!next || next === cur) return;
            const resp = await fetch(`/api/directories/${encodeURIComponent(id)}`, {
                method: 'PATCH',
                headers: { 'content-type': 'application/json' },
                body: JSON.stringify({ label: next }),
            });
            if (!resp.ok) { alert('Rename failed'); return; }
            // Optimistic in-place update; poll will re-confirm.
            if (labelEl) labelEl.textContent = next;
        }
    });

    async function startScan(dirId) {
        const url = dirId ? `/api/scan?dir_id=${encodeURIComponent(dirId)}` : '/api/scan';
        const resp = await fetch(url, { method: 'POST' });
        if (!resp.ok) { alert('Scan failed to start'); return; }
        scheduleNextPoll(0);
    }

    // --- Directory list in-place refresh ---

    async function refreshDirectories() {
        let list;
        try {
            const resp = await fetch('/api/directories');
            if (!resp.ok) return;
            list = await resp.json();
        } catch { return; }

        const active = list.filter(d => !d.removed);
        const table = $('#dirs-table');
        const empty = $('#dirs-empty');
        if (!table || !empty) return;

        if (active.length === 0) {
            table.hidden = true;
            empty.hidden = false;
            return;
        }
        empty.hidden = true;
        table.hidden = false;

        const body = $('#dirs-body');
        const existingRows = new Map();
        for (const tr of body.querySelectorAll('tr[data-dir-id]')) {
            existingRows.set(tr.dataset.dirId, tr);
        }

        // Upsert rows in server order.
        const seen = new Set();
        for (const d of active) {
            const key = String(d.id);
            seen.add(key);
            let row = existingRows.get(key);
            if (!row) {
                row = buildRow(d);
                body.appendChild(row);
            } else {
                updateRow(row, d);
            }
        }
        // Remove rows that no longer apply.
        for (const [key, tr] of existingRows) {
            if (!seen.has(key)) tr.remove();
        }
    }

    function buildRow(d) {
        const tr = document.createElement('tr');
        tr.dataset.dirId = d.id;

        const lblTd = document.createElement('td');
        const lblSpan = document.createElement('span');
        lblSpan.className = 'label-text';
        lblSpan.textContent = d.label;
        const renameBtn = document.createElement('button');
        renameBtn.type = 'button';
        renameBtn.className = 'link btn-rename';
        renameBtn.textContent = 'rename';
        lblTd.append(lblSpan, ' ', renameBtn);

        const pathTd = document.createElement('td');
        pathTd.className = 'mono';
        pathTd.dataset.field = 'path';
        pathTd.textContent = d.path;

        const countTd = document.createElement('td');
        countTd.dataset.field = 'video_count';
        countTd.textContent = d.video_count;

        const statusTd = document.createElement('td');
        statusTd.dataset.field = 'status';
        statusTd.appendChild(badgeFor(d));

        const actionsTd = document.createElement('td');
        actionsTd.dataset.field = 'actions';
        if (!d.removed) {
            const rescan = document.createElement('button');
            rescan.type = 'button';
            rescan.className = 'btn-rescan';
            rescan.textContent = 'Rescan';
            const remove = document.createElement('button');
            remove.type = 'button';
            remove.className = 'danger btn-remove';
            remove.textContent = 'Remove';
            actionsTd.append(rescan, ' ', remove);
        }

        tr.append(lblTd, pathTd, countTd, statusTd, actionsTd);
        return tr;
    }

    function updateRow(row, d) {
        const lblEl = row.querySelector('.label-text');
        if (lblEl && lblEl.textContent !== d.label) lblEl.textContent = d.label;
        const pathTd = row.querySelector('[data-field="path"]');
        if (pathTd && pathTd.textContent !== d.path) pathTd.textContent = d.path;
        const countTd = row.querySelector('[data-field="video_count"]');
        if (countTd && countTd.textContent !== String(d.video_count)) {
            countTd.textContent = d.video_count;
        }
        const statusTd = row.querySelector('[data-field="status"]');
        if (statusTd) {
            statusTd.innerHTML = '';
            statusTd.appendChild(badgeFor(d));
        }
    }

    function badgeFor(d) {
        const span = document.createElement('span');
        span.className = 'badge ' + (d.removed ? 'danger' : 'ok');
        span.textContent = d.removed ? 'removed' : 'active';
        return span;
    }

    // --- Scan + job status polling ---

    const summaryEl = $('#scan-summary');
    const activityCard = $('#activity-card');
    const scanDetail = $('#scan-detail');

    let pollHandle = null;
    let pollInterval = 1500;

    function scheduleNextPoll(delayMs) {
        if (pollHandle) { clearTimeout(pollHandle); pollHandle = null; }
        pollHandle = setTimeout(poll, delayMs);
    }

    async function poll() {
        let s;
        try {
            const resp = await fetch('/api/scan/status');
            if (!resp.ok) throw new Error('status fetch failed');
            s = await resp.json();
        } catch {
            scheduleNextPoll(pollInterval);
            return;
        }

        renderStatus(s);
        await refreshDirectories();

        // Poll briskly while anything is happening; slower when idle.
        pollInterval = s.busy ? 1500 : 5000;
        scheduleNextPoll(pollInterval);
    }

    function renderStatus(s) {
        const jobs = s.jobs || {};
        const incomplete =
            countIncomplete(jobs.probe) +
            countIncomplete(jobs.thumbnail) +
            countIncomplete(jobs.preview);

        let summary;
        if (s.phase === 'walking') {
            summary = `Scanning… files seen ${s.files_seen}, new ${s.new_videos}, changed ${s.changed_videos}, missing ${s.missing_videos}`;
        } else if (s.phase === 'failed') {
            summary = `Scan failed: ${s.error || 'unknown error'}`;
        } else if (incomplete > 0) {
            summary = `Processing ${incomplete} background job${incomplete === 1 ? '' : 's'}`;
        } else if (s.phase === 'done') {
            summary = `Scan complete. new ${s.new_videos}, changed ${s.changed_videos}, missing ${s.missing_videos}. All background jobs done.`;
        } else {
            summary = 'Idle';
        }
        if (summaryEl) summaryEl.textContent = summary;

        const show = s.busy || s.phase === 'walking' || s.phase === 'failed' || incomplete > 0;
        if (activityCard) activityCard.hidden = !show;

        if (scanDetail) {
            if (s.phase === 'walking' || s.phase === 'failed') {
                scanDetail.hidden = false;
                if (s.phase === 'walking') {
                    scanDetail.textContent = `Scan: ${s.files_seen} files seen · ${s.new_videos} new · ${s.changed_videos} changed · ${s.missing_videos} missing`;
                } else {
                    scanDetail.textContent = `Scan failed: ${s.error || 'unknown error'}`;
                }
            } else {
                scanDetail.hidden = true;
            }
        }

        renderJobBars(jobs);
    }

    function countIncomplete(k) {
        if (!k) return 0;
        return (k.pending || 0) + (k.running || 0);
    }

    function renderJobBars(jobs) {
        for (const kind of ['probe', 'thumbnail', 'preview']) {
            const el = document.querySelector(`.job-stat[data-kind="${kind}"]`);
            if (!el) continue;
            const k = jobs[kind] || { pending: 0, running: 0, done: 0, failed: 0 };
            const total = (k.pending || 0) + (k.running || 0) + (k.done || 0) + (k.failed || 0);
            const finished = (k.done || 0) + (k.failed || 0);
            const pct = total > 0 ? (finished / total) * 100 : 0;
            const fill = el.querySelector('.progress-fill');
            if (fill) fill.style.width = pct.toFixed(1) + '%';
            const counts = el.querySelector('.job-counts');
            if (counts) {
                const pieces = [
                    `${finished}/${total}`,
                    k.running ? `${k.running} running` : null,
                    k.pending ? `${k.pending} queued` : null,
                    k.failed ? `<span class="failed">${k.failed} failed</span>` : null,
                ].filter(Boolean);
                counts.innerHTML = pieces.join(' · ');
            }
        }
    }

    // Kick things off.
    scheduleNextPoll(0);
})();
