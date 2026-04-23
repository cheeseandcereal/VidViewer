// Home page: "+ New Collection" modal with name + directory checklist.

(() => {
    const btn = document.getElementById('btn-new-collection');
    const modal = document.getElementById('new-collection-modal');
    const nameInput = document.getElementById('new-collection-name');
    const dirsList = document.getElementById('new-collection-dirs');
    const dirsEmpty = document.getElementById('new-collection-dirs-empty');
    const createBtn = document.getElementById('new-collection-create');
    const cancelBtn = document.getElementById('new-collection-cancel');
    const status = document.getElementById('new-collection-status');
    if (!btn || !modal) return;

    function closeModal() {
        modal.hidden = true;
        nameInput.value = '';
        dirsList.innerHTML = '';
        status.textContent = '';
    }

    async function openModal() {
        modal.hidden = false;
        nameInput.focus();
        dirsList.innerHTML = '';
        dirsEmpty.hidden = true;
        status.textContent = '';
        const resp = await fetch('/api/directories');
        if (!resp.ok) {
            status.textContent = 'Failed to list directories.';
            return;
        }
        const dirs = (await resp.json()).filter(d => !d.removed);
        if (!dirs.length) {
            dirsEmpty.hidden = false;
            return;
        }
        for (const d of dirs) {
            const li = document.createElement('li');
            const label = document.createElement('label');
            const cb = document.createElement('input');
            cb.type = 'checkbox';
            cb.value = String(d.id);
            label.appendChild(cb);
            const text = document.createElement('span');
            text.textContent = ` ${d.label}`;
            label.appendChild(text);
            const path = document.createElement('span');
            path.className = 'muted mono';
            path.style.marginLeft = 'var(--space-2, 0.5rem)';
            path.textContent = d.path;
            label.appendChild(path);
            li.appendChild(label);
            dirsList.appendChild(li);
        }
    }

    btn.addEventListener('click', openModal);
    cancelBtn.addEventListener('click', closeModal);

    createBtn.addEventListener('click', async () => {
        const name = nameInput.value.trim();
        if (!name) {
            status.textContent = 'Please enter a name.';
            nameInput.focus();
            return;
        }
        const directoryIds = Array.from(
            dirsList.querySelectorAll('input[type=checkbox]:checked')
        ).map(cb => parseInt(cb.value, 10)).filter(n => !Number.isNaN(n));
        createBtn.disabled = true;
        status.textContent = 'Creating…';
        const resp = await fetch('/api/collections', {
            method: 'POST',
            headers: { 'content-type': 'application/json' },
            body: JSON.stringify({ name, directory_ids: directoryIds }),
        });
        createBtn.disabled = false;
        if (!resp.ok) {
            status.textContent = 'Failed to create collection.';
            return;
        }
        const created = await resp.json();
        window.location.href = `/collections/${encodeURIComponent(created.id)}`;
    });

    document.addEventListener('keydown', ev => {
        if (modal.hidden) return;
        if (ev.key === 'Escape') closeModal();
    });
})();
