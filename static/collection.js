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
