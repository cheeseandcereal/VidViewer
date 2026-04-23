// Home page: "+ New Collection" button.

(() => {
    const btn = document.getElementById('btn-new-collection');
    if (!btn) return;
    btn.addEventListener('click', async () => {
        const name = prompt('Name for the new collection:');
        if (!name || !name.trim()) return;
        const resp = await fetch('/api/collections', {
            method: 'POST',
            headers: { 'content-type': 'application/json' },
            body: JSON.stringify({ name: name.trim() }),
        });
        if (!resp.ok) {
            alert('Failed to create collection');
            return;
        }
        window.location.reload();
    });
})();
