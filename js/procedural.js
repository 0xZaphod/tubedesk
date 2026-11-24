// Minimal executor for adblock-rust "procedural or action" cosmetic filters
// (uBO syntax such as `:has-text()`, `:upward()`, `:remove()`, `:style()`).
// Input format: [{ selector: [{type, arg}, ...], action?: {type, arg?} }, ...]
function __tdProcedural(filters) {
    if (!filters || filters.length === 0) return;

    const toRegex = (arg) => {
        const m = /^\/(.*)\/([imsu]*)$/.exec(arg);
        if (m) return new RegExp(m[1], m[2]);
        return new RegExp(arg.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'));
    };
    const COMBINATOR = /^\s*[>+~]|^\s+/;

    const ops = {
        'css-selector': (els, arg, first) => {
            if (first) return Array.from(document.querySelectorAll(arg));
            if (COMBINATOR.test(arg)) {
                return els.flatMap((e) => Array.from(e.querySelectorAll(':scope ' + arg)));
            }
            return els.filter((e) => e.matches(arg));
        },
        'has-text': (els, arg) => {
            const re = toRegex(arg);
            return els.filter((e) => re.test(e.textContent || ''));
        },
        'min-text-length': (els, arg) => {
            const n = parseInt(arg, 10) || 0;
            return els.filter((e) => (e.textContent || '').length >= n);
        },
        'matches-attr': (els, arg) => {
            const m = /^"?(.+?)"?(?:="?(.*?)"?)?$/.exec(arg);
            if (!m) return [];
            const nameRe = toRegex(m[1]);
            const valRe = m[2] !== undefined ? toRegex(m[2]) : null;
            return els.filter((e) => Array.from(e.attributes).some(
                (a) => nameRe.test(a.name) && (!valRe || valRe.test(a.value))));
        },
        'matches-css': (els, arg, _f, pseudo = null) => {
            const i = arg.indexOf(':');
            if (i < 0) return [];
            const prop = arg.slice(0, i).trim();
            const re = toRegex(arg.slice(i + 1).trim());
            return els.filter((e) => re.test(getComputedStyle(e, pseudo).getPropertyValue(prop)));
        },
        'matches-css-before': (els, arg) => ops['matches-css'](els, arg, false, '::before'),
        'matches-css-after': (els, arg) => ops['matches-css'](els, arg, false, '::after'),
        'matches-path': (els, arg) => (toRegex(arg).test(location.pathname + location.search) ? els : []),
        'upward': (els, arg) => {
            const n = /^\d+$/.test(arg) ? parseInt(arg, 10) : null;
            return els.map((e) => {
                if (n === null) return e.parentElement && e.parentElement.closest(arg);
                let p = e;
                for (let i = 0; i < n && p; i++) p = p.parentElement;
                return p;
            }).filter(Boolean);
        },
        'xpath': (els, arg, first) => {
            const out = [];
            for (const ctx of first ? [document] : els) {
                const r = document.evaluate(arg, ctx, null, XPathResult.ORDERED_NODE_SNAPSHOT_TYPE, null);
                for (let i = 0; i < r.snapshotLength; i++) {
                    const node = r.snapshotItem(i);
                    if (node.nodeType === 1) out.push(node);
                }
            }
            return out;
        },
    };

    const apply = (el, action) => {
        if (!action) {
            el.style.setProperty('display', 'none', 'important');
            return;
        }
        switch (action.type) {
        case 'remove': el.remove(); break;
        case 'style': el.style.cssText += ';' + action.arg; break;
        case 'remove-attr': el.removeAttribute(action.arg); break;
        case 'remove-class': el.classList.remove(action.arg); break;
        }
    };

    const runAll = () => {
        for (const f of filters) {
            try {
                let els = [];
                f.selector.forEach((op, i) => {
                    const fn = ops[op.type];
                    els = fn ? fn(els, op.arg, i === 0) : [];
                });
                for (const el of new Set(els)) apply(el, f.action);
            } catch (e) { /* ignore malformed filters */ }
        }
    };

    let scheduled = false;
    const schedule = () => {
        if (scheduled) return;
        scheduled = true;
        setTimeout(() => { scheduled = false; runAll(); }, 250);
    };
    const start = () => {
        runAll();
        new MutationObserver(schedule).observe(document.documentElement, { childList: true, subtree: true });
    };
    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', start, { once: true });
    } else {
        start();
    }
}
