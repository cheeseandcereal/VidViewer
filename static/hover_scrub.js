// Hover-scrub: replaces an element's background with the appropriate preview frame
// based on mouse X. Uses the VTT file (#xywh cues) produced by the preview job.
//
// Usage:
//   <div class="hover-scrub" data-video-id="..." data-preview-ok="1"
//        data-duration="123.45" data-cache-bust="1712345678">
//     <img class="poster" src="/thumbs/<id>.jpg?v=...">
//   </div>
//
// The JS attaches to any element with the `hover-scrub` class, parses its VTT on
// first mouseenter, and swaps the poster image for the preview sheet, positioning
// it as a CSS sprite so the correct tile fills the container at any size.

(() => {
    const parsedCache = new Map(); // video_id -> { cues, sheetUrl, cols, rows }

    function parseVtt(text) {
        const lines = text.split(/\r?\n/);
        const cues = [];
        let i = 0;
        while (i < lines.length && !lines[i].includes('-->')) i++;
        while (i < lines.length) {
            const arrow = lines[i];
            if (!arrow.includes('-->')) { i++; continue; }
            const [startStr, endStr] = arrow.split('-->').map(s => s.trim());
            const start = parseTime(startStr);
            const end = parseTime(endStr);
            i++;
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
        const parts = s.split(':').map(Number);
        if (parts.length === 3) return parts[0] * 3600 + parts[1] * 60 + parts[2];
        if (parts.length === 2) return parts[0] * 60 + parts[1];
        return Number(s) || 0;
    }

    // Derive the sprite grid layout (cols, rows, tile dimensions) from the cue set.
    function computeGrid(cues) {
        let tileW = 0;
        let tileH = 0;
        let maxX = 0;
        let maxY = 0;
        for (const c of cues) {
            if (c.w > tileW) tileW = c.w;
            if (c.h > tileH) tileH = c.h;
            if (c.x > maxX) maxX = c.x;
            if (c.y > maxY) maxY = c.y;
        }
        const cols = tileW > 0 ? Math.floor(maxX / tileW) + 1 : 1;
        const rows = tileH > 0 ? Math.floor(maxY / tileH) + 1 : 1;
        return { cols, rows, tileW, tileH };
    }

    async function loadCues(videoId, cacheBust) {
        if (parsedCache.has(videoId)) return parsedCache.get(videoId);
        const url = `/previews/${encodeURIComponent(videoId)}.vtt?v=${encodeURIComponent(cacheBust)}`;
        const resp = await fetch(url);
        if (!resp.ok) return null;
        const text = await resp.text();
        const cues = parseVtt(text);
        if (cues.length === 0) return null;
        const grid = computeGrid(cues);
        const data = {
            cues,
            sheetUrl: cues[0].url,
            cols: grid.cols,
            rows: grid.rows,
            tileW: grid.tileW,
            tileH: grid.tileH,
        };
        parsedCache.set(videoId, data);
        return data;
    }

    function findCueAt(cues, t) {
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

        let cuesData = null;

        el.addEventListener('mouseenter', async () => {
            if (!cuesData) cuesData = await loadCues(videoId, cacheBust);
        });

        el.addEventListener('mousemove', ev => {
            if (!cuesData) return;
            const rect = el.getBoundingClientRect();
            const x = Math.max(0, Math.min(1, (ev.clientX - rect.left) / rect.width));
            const t = x * duration;
            const c = findCueAt(cuesData.cues, t);
            if (!c) return;

            const { cols, rows, tileW, tileH } = cuesData;
            const col = tileW > 0 ? Math.round(c.x / tileW) : 0;
            const row = tileH > 0 ? Math.round(c.y / tileH) : 0;

            // CSS-sprite percentage layout: scale the background to be `cols x rows`
            // container-sizes large, then position the sheet so the requested tile
            // is what shows through the container. Paint it on the container itself
            // (over the poster image, which stays as the fallback underneath).
            el.style.backgroundImage = `url("${c.url}")`;
            el.style.backgroundSize = `${cols * 100}% ${rows * 100}%`;
            const bgX = cols > 1 ? (col / (cols - 1)) * 100 : 0;
            const bgY = rows > 1 ? (row / (rows - 1)) * 100 : 0;
            el.style.backgroundPosition = `${bgX}% ${bgY}%`;
            el.style.backgroundRepeat = 'no-repeat';

            if (poster) poster.style.opacity = '0';
        });

        el.addEventListener('mouseleave', () => {
            el.style.backgroundImage = '';
            el.style.backgroundSize = '';
            el.style.backgroundPosition = '';
            el.style.backgroundRepeat = '';
            if (poster) poster.style.opacity = '';
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
