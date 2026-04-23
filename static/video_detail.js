// Video detail page. Play / Resume / Re-roll / Add-to-collection.
//
// Actual mpv launching is wired in step 12 via POST /api/videos/:id/play.

(() => {
    const d = window.VIDEO_DETAIL || {};
    const playBtn = document.getElementById('btn-play');
    const resumeBtn = document.getElementById('btn-resume');
    const rerollBtn = document.getElementById('btn-reroll');
    const addToBtn = document.getElementById('btn-add-to');

    async function play(startSecs) {
        if (!d.video_id) return;
        let url = `/api/videos/${encodeURIComponent(d.video_id)}/play`;
        if (typeof startSecs === 'number' && startSecs > 0) {
            url += `?start=${encodeURIComponent(startSecs)}`;
        }
        const resp = await fetch(url, { method: 'POST' });
        if (!resp.ok) {
            const body = await resp.text().catch(() => '');
            alert('Play failed' + (body ? ': ' + body : ''));
        }
    }

    async function reroll() {
        if (!d.from_cid) return;
        const resp = await fetch(`/api/collections/${encodeURIComponent(d.from_cid)}/random`);
        if (resp.status === 404) {
            alert('Collection is empty');
            return;
        }
        if (!resp.ok) {
            alert('Re-roll failed');
            return;
        }
        const { video_id } = await resp.json();
        window.location.href = `/videos/${encodeURIComponent(video_id)}?cid=${encodeURIComponent(d.from_cid)}`;
    }

    async function addToCollection() {
        const resp = await fetch('/api/collections?kind=custom');
        if (!resp.ok) { alert('Could not list collections'); return; }
        const custom = await resp.json();
        if (!custom.length) {
            alert('No custom collections yet. Create one on the Home page.');
            return;
        }
        const names = custom.map((c, i) => `${i + 1}. ${c.name}`).join('\n');
        const answer = prompt('Add to which collection?\n\n' + names + '\n\nEnter a number:');
        if (!answer) return;
        const idx = parseInt(answer, 10) - 1;
        const target = custom[idx];
        if (!target) { alert('Invalid selection'); return; }
        const addResp = await fetch(`/api/collections/${encodeURIComponent(target.id)}/videos`, {
            method: 'POST',
            headers: { 'content-type': 'application/json' },
            body: JSON.stringify({ video_id: d.video_id }),
        });
        if (!addResp.ok) { alert('Add failed'); return; }
        alert(`Added to "${target.name}"`);
    }

    if (playBtn) playBtn.addEventListener('click', () => play(0));
    if (resumeBtn) resumeBtn.addEventListener('click', () => play(d.resume_secs || 0));
    if (rerollBtn) rerollBtn.addEventListener('click', reroll);
    if (addToBtn) addToBtn.addEventListener('click', addToCollection);

    document.addEventListener('keydown', ev => {
        if (ev.defaultPrevented) return;
        const t = ev.target;
        if (t instanceof HTMLElement) {
            const tag = t.tagName;
            if (tag === 'INPUT' || tag === 'TEXTAREA' || t.isContentEditable) return;
        }
        if (ev.key === 'Escape') {
            if (d.from_cid) {
                window.location.href = `/collections/${encodeURIComponent(d.from_cid)}`;
            } else {
                window.location.href = '/';
            }
        } else if (ev.key === ' ' || ev.key === 'Enter') {
            ev.preventDefault();
            play(0);
        } else if (ev.key === 'r' || ev.key === 'R') {
            if (d.from_cid) {
                ev.preventDefault();
                reroll();
            }
        }
    });
})();
