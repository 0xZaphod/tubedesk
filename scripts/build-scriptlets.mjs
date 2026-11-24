#!/usr/bin/env node
// Converts uBlock Origin's scriptlets (src/js/resources/scriptlets.js) into the
// `Resource` JSON format understood by Brave's adblock-rust engine.
//
// Usage:
//   node scripts/build-scriptlets.mjs [path/to/uBlock checkout]
//
// Without an argument it shallow-clones uBO at UBO_TAG into a temp dir.
// Output: resources/ubo-scriptlets.json (committed; GPL-3.0, from uBO).

import { execFileSync } from 'node:child_process';
import { mkdtempSync, writeFileSync, rmSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve, dirname } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const UBO_TAG = process.env.UBO_TAG || '1.77.0';
// Must match TRUSTED_PERMISSION in src/adblock.rs.
const TRUSTED_PERMISSION = 1;

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const out = join(root, 'resources', 'ubo-scriptlets.json');

let uboDir = process.argv[2];
let tmp;
if (!uboDir) {
    tmp = mkdtempSync(join(tmpdir(), 'ubo-'));
    uboDir = join(tmp, 'uBlock');
    console.error(`cloning uBlock Origin ${UBO_TAG}…`);
    execFileSync('git', ['clone', '-q', '--depth', '1', '--branch', UBO_TAG,
        'https://github.com/gorhill/uBlock.git', uboDir], { stdio: 'inherit' });
}

const entry = join(resolve(uboDir), 'src', 'js', 'resources', 'scriptlets.js');
if (!existsSync(entry)) {
    console.error(`not found: ${entry}`);
    process.exit(1);
}

const { builtinScriptlets } = await import(pathToFileURL(entry).href);

const b64 = (s) => Buffer.from(s, 'utf8').toString('base64');
const resources = [];
const seen = new Set();

for (const s of builtinScriptlets) {
    if (typeof s.fn !== 'function' || seen.has(s.name)) { continue; }
    seen.add(s.name);
    const src = s.fn.toString();
    // Scriptlets are invoked by name, so adblock-rust needs `function name(`.
    // `.fn` helpers are only pasted in as dependencies, so classes and async
    // functions are fine there.
    const isHelper = s.name.endsWith('.fn');
    const ok = isHelper
        ? /^(async\s+)?function\s*\*?\s*[^\s(){}]+\s*\(|^class\s+[^\s{}]+/.test(src)
        : /^function\s+[^\s(){}]+\s*\(/.test(src);
    if (!ok) {
        console.error(`skip ${s.name}: not a named function/class declaration`);
        continue;
    }
    const r = {
        name: s.name,
        aliases: s.aliases || [],
        kind: { mime: s.name.endsWith('.fn') ? 'fn/javascript' : 'application/javascript' },
        content: b64(src),
    };
    const deps = (s.dependencies || []).filter((d) => typeof d === 'string');
    if (deps.length) { r.dependencies = deps; }
    if (s.requiresTrust) { r.permission = TRUSTED_PERMISSION; }
    resources.push(r);
}

writeFileSync(out, JSON.stringify(resources) + '\n');
console.error(`wrote ${resources.length} resources (uBO ${UBO_TAG}) → ${out}`);
if (tmp) { rmSync(tmp, { recursive: true, force: true }); }
