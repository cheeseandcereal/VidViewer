// Settings page: directory management + directory picker modal + live status polling.
//
// No framework — only fetch + small DOM helpers. Data comes from /api/directories,
// /api/fs/list, /api/directories/jobs, and /api/scan/status. The page never reloads
// itself; all updates mutate the DOM in place so the user's scroll position and
// modal state are preserved while background work progresses.

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
        closePicker();
        refreshDirectories();
        scheduleNextPoll(0);
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

    // --- Remove modal ---

    const removeModal = $('#remove-modal');
    const removeModalLabel = $('#remove-modal-label');
    const btnRemoveSoft = $('#btn-remove-soft');
    const btnRemoveHard = $('#btn-remove-hard');
    const btnRemoveCancel = $('#btn-remove-cancel');
    let removeTargetId = null;

    function openRemoveModal(id, label) {
        removeTargetId = id;
        if (removeModalLabel) removeModalLabel.textContent = label || '';
        if (removeModal) removeModal.hidden = false;
    }
    function closeRemoveModal() {
        removeTargetId = null;
        if (removeModal) removeModal.hidden = true;
    }
    async function performRemove(mode) {
        if (!removeTargetId) return;
        const id = removeTargetId;
        const url = `/api/directories/${encodeURIComponent(id)}?mode=${encodeURIComponent(mode)}`;
        const resp = await fetch(url, { method: 'DELETE' });
        if (!resp.ok) {
            alert(`${mode === 'hard' ? 'Delete' : 'Remove'} failed`);
            return;
        }
        closeRemoveModal();
        refreshDirectories();
        scheduleNextPoll(0);
    }
    if (btnRemoveSoft) btnRemoveSoft.addEventListener('click', () => performRemove('soft'));
    if (btnRemoveHard) btnRemoveHard.addEventListener('click', () => {
        if (!confirm(
            'Permanently delete this directory?\n\n' +
            'All videos in this directory, their watch history, memberships in custom ' +
            'collections, and cached thumbnails/previews will be removed from disk.\n\n' +
            'This cannot be undone.'
        )) return;
        performRemove('hard');
    });
    if (btnRemoveCancel) btnRemoveCancel.addEventListener('click', closeRemoveModal);

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
            const labelEl = row.querySelector('.label-text');
            const label = labelEl ? labelEl.textContent : '';
            openRemoveModal(id, label);
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

        const activityTd = document.createElement('td');
        activityTd.dataset.field = 'activity';
        const idle = document.createElement('span');
        idle.className = 'activity-idle muted';
        idle.textContent = 'idle';
        activityTd.appendChild(idle);

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

        tr.append(lblTd, pathTd, countTd, activityTd, actionsTd);
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
    }

    function renderDirectoryActivity(jobsByDir) {
        const rows = document.querySelectorAll('tr[data-dir-id]');
        for (const row of rows) {
            const id = row.dataset.dirId;
            const cell = row.querySelector('[data-field="activity"]');
            if (!cell) continue;
            const s = jobsByDir[id];
            cell.innerHTML = '';
            if (!s || (s.probe_incomplete + s.thumbnail_incomplete + s.preview_incomplete === 0 && !s.failed)) {
                const span = document.createElement('span');
                span.className = 'activity-idle muted';
                span.textContent = 'idle';
                cell.appendChild(span);
                continue;
            }
            const pieces = [];
            if (s.probe_incomplete) pieces.push(`probing ${s.probe_incomplete}`);
            if (s.thumbnail_incomplete) {
                const done = Math.max(0, s.video_total - s.thumbnail_pending_videos);
                pieces.push(`thumbs ${done}/${s.video_total}`);
            }
            if (s.preview_incomplete) {
                // preview denominator = videos with a usable duration
                const denom = Math.max(s.preview_pending_videos, 0) + Math.max(s.video_total - s.preview_pending_videos, 0);
                const done = Math.max(0, denom - s.preview_pending_videos);
                pieces.push(`previews ${done}/${denom}`);
            }
            if (s.failed) pieces.push(`<span class="failed">${s.failed} failed</span>`);

            const wrap = document.createElement('div');
            wrap.className = 'activity-cell';
            wrap.innerHTML = pieces.join(' · ');

            // Small progress bar: fraction of videos that have both thumb + preview ready.
            if (s.video_total > 0) {
                const notReady = Math.max(s.thumbnail_pending_videos, s.preview_pending_videos);
                const ready = Math.max(0, s.video_total - notReady);
                const pct = (ready / s.video_total) * 100;
                const bar = document.createElement('div');
                bar.className = 'progress-bar';
                const fill = document.createElement('div');
                fill.className = 'progress-fill';
                fill.style.width = pct.toFixed(1) + '%';
                bar.appendChild(fill);
                wrap.appendChild(bar);
            }
            cell.appendChild(wrap);
        }
    }

    // --- Scan + per-directory job status polling ---

    const summaryEl = $('#scan-summary');

    let pollHandle = null;
    let pollInterval = 1500;

    function scheduleNextPoll(delayMs) {
        if (pollHandle) { clearTimeout(pollHandle); pollHandle = null; }
        pollHandle = setTimeout(poll, delayMs);
    }

    async function poll() {
        let scan = {};
        let jobsByDir = {};
        try {
            const [s, j] = await Promise.all([
                fetch('/api/scan/status').then(r => r.ok ? r.json() : {}),
                fetch('/api/directories/jobs').then(r => r.ok ? r.json() : {}),
            ]);
            scan = s || {};
            jobsByDir = j || {};
        } catch {
            scheduleNextPoll(pollInterval);
            return;
        }

        await refreshDirectories();
        renderDirectoryActivity(jobsByDir);
        renderScanSummary(scan, jobsByDir);

        const anyBusy = Object.values(jobsByDir).some(
            v => (v.probe_incomplete + v.thumbnail_incomplete + v.preview_incomplete) > 0
        );
        const scanning = scan.phase === 'walking';
        pollInterval = (anyBusy || scanning) ? 1500 : 5000;
        scheduleNextPoll(pollInterval);
    }

    function renderScanSummary(scan, jobsByDir) {
        if (!summaryEl) return;
        const scanning = scan.phase === 'walking';
        const anyBusy = Object.values(jobsByDir).some(
            v => (v.probe_incomplete + v.thumbnail_incomplete + v.preview_incomplete) > 0
        );
        if (scan.phase === 'failed') {
            summaryEl.textContent = `Scan failed: ${scan.error || 'unknown error'}`;
        } else if (scanning) {
            summaryEl.textContent =
                `Scanning… files seen ${scan.files_seen || 0}, new ${scan.new_videos || 0}, changed ${scan.changed_videos || 0}, missing ${scan.missing_videos || 0}`;
        } else if (anyBusy) {
            summaryEl.textContent = 'Processing per-directory jobs';
        } else {
            summaryEl.textContent = 'Idle';
        }
    }

    scheduleNextPoll(0);
})();
