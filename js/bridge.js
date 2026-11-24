// Injected into music.youtube.com at document start.
// - reports playback state to Rust (MPRIS / media keys / tray)
// - exposes window.__tubedesk.control() for Rust to drive the player
// - audio-only mode
// - last-resort ad skipper for ads that get past the filter lists
(function () {
    'use strict';
    if (window.top !== window || location.hostname !== 'music.youtube.com') return;

    const AUDIO_ONLY = __TD_AUDIO_ONLY__;
    const $ = (sel) => document.querySelector(sel);
    const video = () => $('#movie_player video') || $('video');
    const player = () => $('#movie_player');

    // ---- Rust IPC -------------------------------------------------------
    const invoke = (cmd, args) => {
        const t = window.__TAURI_INTERNALS__;
        if (t && t.invoke) t.invoke(cmd, args).catch(() => {});
    };

    // ---- state reporting ------------------------------------------------
    let last = '';
    const report = (force) => {
        const v = video();
        const md = navigator.mediaSession && navigator.mediaSession.metadata;
        const art = md && md.artwork && md.artwork.length
            ? md.artwork[md.artwork.length - 1].src : null;
        const state = {
            title: md ? md.title : null,
            artist: md ? md.artist : null,
            album: md ? md.album : null,
            artwork: art,
            playing: !!(v && !v.paused && !v.ended),
            position: v ? v.currentTime || 0 : 0,
            duration: v && isFinite(v.duration) ? v.duration : 0,
            ad: isAdShowing(),
        };
        // Only send when something other than position changed, or periodically.
        const key = JSON.stringify([state.title, state.artist, state.playing, state.ad, Math.round(state.duration)]);
        if (!force && key === last) return;
        last = key;
        invoke('player_state', { player: state });
    };

    // ---- controls (called from Rust via eval) ---------------------------
    const click = (sel) => { const b = $(sel); if (b) { b.click(); return true; } return false; };
    const control = (cmd, arg) => {
        const v = video();
        switch (cmd) {
        case 'play': if (v && v.paused) click('#play-pause-button') || v.play(); break;
        case 'pause': if (v && !v.paused) click('#play-pause-button') || v.pause(); break;
        case 'toggle': click('#play-pause-button') || (v && (v.paused ? v.play() : v.pause())); break;
        case 'next': click('ytmusic-player-bar .next-button'); break;
        case 'previous': click('ytmusic-player-bar .previous-button'); break;
        case 'stop': if (v) v.pause(); break;
        case 'seek': if (v) v.currentTime = Math.max(0, v.currentTime + Number(arg || 0)); break;
        case 'position': if (v) v.currentTime = Math.max(0, Number(arg || 0)); break;
        }
        setTimeout(() => report(true), 300);
    };
    Object.defineProperty(window, '__tubedesk', { value: Object.freeze({ control, report }) });

    // ---- ads: last-resort skipper ---------------------------------------
    function isAdShowing() {
        const p = player();
        return !!(p && (p.classList.contains('ad-showing') || p.classList.contains('ad-interrupting')));
    }
    // Seeking to the end makes YouTube restart the ad, so fast-forward muted instead.
    let adActive = false;
    const handleAd = () => {
        const v = video();
        if (!v) return;
        if (isAdShowing()) {
            adActive = true;
            v.muted = true;
            if (v.playbackRate < 16) v.playbackRate = 16;
            click('.ytp-ad-skip-button, .ytp-ad-skip-button-modern, .ytp-skip-ad-button');
        } else if (adActive) {
            adActive = false;
            v.muted = false;
            v.playbackRate = 1;
        }
    };

    // ---- audio-only ------------------------------------------------------
    const AUDIO_CSS = `
        /* never render video frames; keep album art visible instead */
        ytmusic-player #song-video { visibility: hidden !important; }
        ytmusic-player #song-image { display: block !important; visibility: visible !important; opacity: 1 !important; }
        ytmusic-av-toggle { display: none !important; }
    `;
    const audioOnly = () => {
        // Prefer the "Song" (audio track) version of music videos when YT Music offers one.
        const toggle = $('ytmusic-av-toggle');
        if (toggle && toggle.getAttribute('playback-mode') !== 'ATV_PREFERRED') {
            click('ytmusic-av-toggle .song-button');
        }
        // For videos with no audio-only version, request the smallest video
        // stream so as little video as possible is downloaded and decoded.
        const p = player();
        if (p && typeof p.setPlaybackQualityRange === 'function') {
            try { p.setPlaybackQualityRange('tiny', 'tiny'); } catch (e) { /* ignore */ }
        }
    };

    // ---- wiring ----------------------------------------------------------
    const start = () => {
        if (AUDIO_ONLY) {
            const s = document.createElement('style');
            s.id = 'tubedesk-audio-only';
            s.textContent = AUDIO_CSS;
            document.head.appendChild(s);
        }
        let hooked = null;
        setInterval(() => {
            const v = video();
            if (v && v !== hooked) {
                hooked = v;
                for (const ev of ['play', 'pause', 'ended', 'loadedmetadata', 'durationchange']) {
                    v.addEventListener(ev, () => report(false));
                }
                v.addEventListener('seeked', () => report(true));
            }
            handleAd();
            if (AUDIO_ONLY) audioOnly();
            report(false);
        }, 1000);
        setInterval(handleAd, 250);
        // Periodic forced report keeps the MPRIS position roughly in sync.
        setInterval(() => report(true), 5000);
    };
    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', start, { once: true });
    } else {
        start();
    }
})();
