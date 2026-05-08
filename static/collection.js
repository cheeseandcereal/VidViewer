// Collection page: Random, rename/delete for custom, manage included directories.

(() => {
    const grid = document.querySelector('.video-grid');
    const dirRow = document.querySelector('.coll-dirs');
    // collectionId is available whenever the grid is rendered; for empty custom
    // collections there is no grid, so fall back to the dir-row element.
    const cid = grid?.dataset.collectionId || dirRow?.dataset.collectionId;
    const randomBtn = document.getElementById('btn-random');
    const renameBtn = document.getElementById('btn-rename');
    const deleteBtn = document.getElementById('btn-delete');
    const sortBtn = document.getElementById('btn-sort');
    const sortLabel = document.getElementById('btn-sort-label');

    // ---- Sort toggle ----
    //
    // The server renders videos alphabetically ascending (A -> Z). The toggle
    // flips the DOM order client-side only — no refetch — and persists the
    // user's choice in localStorage so it sticks across navigation. Default
    // direction is descending (Z -> A).
    const SORT_KEY = 'vv.collection.sortDir';
    function readSortDir() {
        try {
            const v = localStorage.getItem(SORT_KEY);
            return v === 'asc' ? 'asc' : 'desc';
        } catch {
            return 'desc';
        }
    }
    function writeSortDir(dir) {
        try { localStorage.setItem(SORT_KEY, dir); } catch { /* ignore */ }
    }
    function applySort(dir) {
        if (!grid) return;
        // Snapshot the children, sort by filename (case-insensitive), then
        // reattach in the desired order. We read the filename from the card's
        // `.video-filename` text node to match exactly what the user sees.
        const cards = Array.from(grid.querySelectorAll('.video-card'));
        if (!cards.length) return;
        const keyed = cards.map(card => {
            const name = (card.querySelector('.video-filename')?.textContent || '').trim();
            return { card, key: name.toLowerCase() };
        });
        keyed.sort((a, b) => {
            if (a.key < b.key) return dir === 'asc' ? -1 : 1;
            if (a.key > b.key) return dir === 'asc' ? 1 : -1;
            return 0;
        });
        // Re-attach in sorted order. appendChild on an already-attached node
        // moves it, so the grid ends up with children in sorted sequence
        // without any layout thrash.
        for (const { card } of keyed) grid.appendChild(card);
    }
    function updateSortLabel(dir) {
        if (!sortLabel) return;
        sortLabel.textContent = dir === 'asc' ? 'A → Z' : 'Z → A';
    }

    let sortDir = readSortDir();
    updateSortLabel(sortDir);
    applySort(sortDir);

    if (sortBtn) {
        sortBtn.addEventListener('click', () => {
            sortDir = sortDir === 'asc' ? 'desc' : 'asc';
            writeSortDir(sortDir);
            updateSortLabel(sortDir);
            applySort(sortDir);
        });
    }

    // ---- Length filter ----
    //
    // Client-side only. Reads `data-duration` (seconds, float) from each
    // card's `.video-thumb` and toggles a `.filtered-out` class. Cards with
    // unknown duration (`data-duration` missing, NaN, or <= 0) are always
    // shown regardless of the selected threshold. Selection persists
    // globally across collection pages in localStorage.
    //
    // The class toggle (rather than the `hidden` attribute) is required
    // because `.video-card { display: block }` has higher specificity than
    // the UA `[hidden] { display: none }` rule, so the attribute alone
    // wouldn't hide anything.
    const lengthSelect = document.getElementById('filter-length');
    const LEN_KEY = 'vv.collection.lengthFilter';
    function readLengthFilter() {
        try {
            return localStorage.getItem(LEN_KEY) || 'any';
        } catch {
            return 'any';
        }
    }
    function writeLengthFilter(val) {
        try { localStorage.setItem(LEN_KEY, val); } catch { /* ignore */ }
    }
    function parseThreshold(v) {
        if (!v || v === 'any') return null;
        const n = Number(v);
        return Number.isFinite(n) ? n : null;
    }
    function applyLengthFilter(v) {
        if (!grid) return;
        const threshold = parseThreshold(v);
        const cards = grid.querySelectorAll('.video-card');
        for (const card of cards) {
            if (threshold === null) {
                card.classList.remove('filtered-out');
                continue;
            }
            const durStr = card.querySelector('.video-thumb')?.dataset.duration;
            const dur = Number(durStr);
            if (!Number.isFinite(dur) || dur <= 0) {
                // Unknown duration — always visible.
                card.classList.remove('filtered-out');
                continue;
            }
            const keep = dur > threshold;
            card.classList.toggle('filtered-out', !keep);
        }
    }

    if (lengthSelect) {
        const stored = readLengthFilter();
        // Guard against stale keys that no longer match an <option>.
        const valid = Array.from(lengthSelect.options).some(o => o.value === stored);
        const initial = valid ? stored : 'any';
        lengthSelect.value = initial;
        applyLengthFilter(initial);
        lengthSelect.addEventListener('change', () => {
            const val = lengthSelect.value;
            writeLengthFilter(val);
            applyLengthFilter(val);
        });
    }

    async function goRandom() {
        if (!cid) return;
        const resp = await fetch(`/api/collections/${encodeURIComponent(cid)}/random`);
        if (resp.status === 404) {
            alert('No playable videos in this collection.');
            return;
        }
        if (!resp.ok) {
            alert('Random failed');
            return;
        }
        const { video_id } = await resp.json();
        window.location.href = `/videos/${encodeURIComponent(video_id)}?cid=${encodeURIComponent(cid)}`;
    }

    if (randomBtn) randomBtn.addEventListener('click', goRandom);

    // 'R' keyboard shortcut — skip when focus is in text inputs.
    document.addEventListener('keydown', ev => {
        if (ev.defaultPrevented) return;
        if (ev.key !== 'r' && ev.key !== 'R') return;
        const t = ev.target;
        if (t instanceof HTMLElement) {
            const tag = t.tagName;
            if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT' || t.isContentEditable) return;
        }
        ev.preventDefault();
        goRandom();
    });

    if (renameBtn) renameBtn.addEventListener('click', async () => {
        const cur = document.querySelector('.collection-header h1').textContent.trim();
        const next = prompt('New name:', cur);
        if (!next || next === cur) return;
        const resp = await fetch(`/api/collections/${encodeURIComponent(cid)}`, {
            method: 'PATCH',
            headers: { 'content-type': 'application/json' },
            body: JSON.stringify({ name: next.trim() }),
        });
        if (!resp.ok) {
            alert('Rename failed');
            return;
        }
        window.location.reload();
    });

    if (deleteBtn) deleteBtn.addEventListener('click', async () => {
        if (!confirm('Delete this collection? The underlying videos and directories are not deleted.')) return;
        const resp = await fetch(`/api/collections/${encodeURIComponent(cid)}`, { method: 'DELETE' });
        if (!resp.ok) {
            alert('Delete failed');
            return;
        }
        window.location.href = '/';
    });

    // ---- Member directory management (custom collections only) ----

    if (dirRow) {
        // Wire up remove buttons on each chip.
        dirRow.querySelectorAll('.coll-dir-chip').forEach(chip => {
            const did = chip.dataset.directoryId;
            const removeBtn = chip.querySelector('.chip-remove');
            if (!removeBtn) return;
            removeBtn.addEventListener('click', async ev => {
                ev.preventDefault();
                const label = chip.querySelector('[data-directory-label]')?.textContent?.trim() || 'this directory';
                if (!confirm(`Remove "${label}" from this collection? Videos and watch history are unaffected.`)) return;
                const resp = await fetch(
                    `/api/collections/${encodeURIComponent(cid)}/directories/${encodeURIComponent(did)}`,
                    { method: 'DELETE' },
                );
                if (!resp.ok) {
                    alert('Remove failed');
                    return;
                }
                window.location.reload();
            });
        });

        // Populate the "Add directory" <select> with eligible directories.
        const select = document.getElementById('add-directory-select');
        if (select) {
            const includedIds = new Set(
                Array.from(dirRow.querySelectorAll('.coll-dir-chip'))
                    .map(c => c.dataset.directoryId),
            );
            fetch('/api/directories').then(async resp => {
                if (!resp.ok) return;
                const all = await resp.json();
                for (const d of all) {
                    if (d.removed) continue;
                    if (includedIds.has(String(d.id))) continue;
                    const opt = document.createElement('option');
                    opt.value = String(d.id);
                    opt.textContent = d.label;
                    select.appendChild(opt);
                }
            });
            select.addEventListener('change', async () => {
                const did = select.value;
                if (!did) return;
                select.disabled = true;
                const resp = await fetch(
                    `/api/collections/${encodeURIComponent(cid)}/directories`,
                    {
                        method: 'POST',
                        headers: { 'content-type': 'application/json' },
                        body: JSON.stringify({ directory_id: parseInt(did, 10) }),
                    },
                );
                select.disabled = false;
                if (!resp.ok) {
                    alert('Add failed');
                    select.value = '';
                    return;
                }
                window.location.reload();
            });
        }
    }
})();
