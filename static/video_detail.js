// Video detail page. Play / Resume / Re-roll.

(() => {
    const d = window.VIDEO_DETAIL || {};
    const playBtn = document.getElementById('btn-play');
    const resumeBtn = document.getElementById('btn-resume');
    const rerollBtn = document.getElementById('btn-reroll');

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

    if (playBtn) playBtn.addEventListener('click', () => play(0));
    if (resumeBtn) resumeBtn.addEventListener('click', () => play(d.resume_secs || 0));
    if (rerollBtn) rerollBtn.addEventListener('click', reroll);

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
