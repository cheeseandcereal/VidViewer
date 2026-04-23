// Hover-scrub: replaces an element's background with the appropriate preview frame
// based on mouse X. Uses the VTT file (#xywh cues) produced by the preview job.
//
// Usage:
//   <div class="hover-scrub" data-video-id="..." data-preview-ok="1"
//        data-duration="123.45" data-cache-bust="1712345678">
//     <img class="poster" src="/thumbs/<id>.jpg?v=...">
//   </div>
//
// The JS attaches to any element with the `hover-scrub` class, parses its VTT on first
// mouseenter, and sets `background-image` + `background-position` on the poster image
// as the mouse moves. On mouseleave, the background is cleared so the poster shows.

(() => {
    const parsedCache = new Map(); // video_id -> { image, cues }

    function parseVtt(text) {
        const lines = text.split(/\r?\n/);
        const cues = [];
        let i = 0;
        // Skip WEBVTT header.
        while (i < lines.length && !lines[i].includes('-->')) i++;
        while (i < lines.length) {
            const arrow = lines[i];
            if (!arrow.includes('-->')) { i++; continue; }
            const [startStr, endStr] = arrow.split('-->').map(s => s.trim());
            const start = parseTime(startStr);
            const end = parseTime(endStr);
            i++;
            // Next non-empty line: the image reference.
            let ref = '';
            while (i < lines.length && lines[i].trim()) {
                ref = lines[i].trim();
                i++;
            }
            const m = ref.match(/^(.*?)#xywh=(\d+),(\d+),(\d+),(\d+)$/);
            if (!m) continue;
            cues.push({
                start,
                end,
                url: m[1],
                x: parseInt(m[2], 10),
                y: parseInt(m[3], 10),
                w: parseInt(m[4], 10),
                h: parseInt(m[5], 10),
            });
        }
        return cues;
    }

    function parseTime(s) {
        // Accept HH:MM:SS.mmm or MM:SS.mmm
        const parts = s.split(':').map(Number);
        if (parts.length === 3) {
            return parts[0] * 3600 + parts[1] * 60 + parts[2];
        }
        if (parts.length === 2) {
            return parts[0] * 60 + parts[1];
        }
        return Number(s) || 0;
    }

    async function loadCues(videoId, cacheBust) {
        if (parsedCache.has(videoId)) return parsedCache.get(videoId);
        const url = `/previews/${encodeURIComponent(videoId)}.vtt?v=${encodeURIComponent(cacheBust)}`;
        const resp = await fetch(url);
        if (!resp.ok) return null;
        const text = await resp.text();
        const cues = parseVtt(text);
        if (cues.length === 0) return null;
        const data = { cues, sheetUrl: cues[0].url };
        parsedCache.set(videoId, data);
        return data;
    }

    function findCueAt(cues, t) {
        // Linear scan is fine for <= ~100 cues.
        for (const c of cues) {
            if (t >= c.start && t < c.end) return c;
        }
        return cues[cues.length - 1];
    }

    function attach(el) {
        const videoId = el.dataset.videoId;
        const previewOk = el.dataset.previewOk === '1';
        const duration = parseFloat(el.dataset.duration || '0');
        const cacheBust = el.dataset.cacheBust || '0';
        if (!videoId || !previewOk || !duration) return;

        const poster = el.querySelector('img, .poster');
        if (!poster) return;

        let cuesData = null;
        let origSrc = null;

        el.addEventListener('mouseenter', async () => {
            if (!cuesData) cuesData = await loadCues(videoId, cacheBust);
            if (!cuesData) return;
            origSrc = origSrc || poster.getAttribute('src') || '';
        });

        el.addEventListener('mousemove', ev => {
            if (!cuesData) return;
            const rect = el.getBoundingClientRect();
            const x = Math.max(0, Math.min(1, (ev.clientX - rect.left) / rect.width));
            const t = x * duration;
            const c = findCueAt(cuesData.cues, t);
            if (!c) return;

            // Apply the sprite as a background on the poster element, hiding its src image.
            poster.style.backgroundImage = `url("${c.url}")`;
            poster.style.backgroundPosition = `-${c.x}px -${c.y}px`;
            poster.style.backgroundSize = 'auto';
            poster.style.backgroundRepeat = 'no-repeat';
            poster.style.width = `${c.w}px`;
            poster.style.height = `${c.h}px`;
            // Hide the <img>'s own content by clearing its src temporarily.
            if (poster.tagName === 'IMG') {
                poster.setAttribute('src', '');
            }
        });

        el.addEventListener('mouseleave', () => {
            poster.style.backgroundImage = '';
            poster.style.width = '';
            poster.style.height = '';
            if (poster.tagName === 'IMG' && origSrc) {
                poster.setAttribute('src', origSrc);
            }
        });
    }

    function initAll() {
        document.querySelectorAll('.hover-scrub').forEach(attach);
    }

    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', initAll);
    } else {
        initAll();
    }
})();
