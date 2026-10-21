import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { runInNewContext } from 'node:vm';

// Exercise the listener passed to WebviewWindowBuilder, not a copied predicate.
const source = readFileSync(new URL('../src-tauri/src/lib.rs', import.meta.url), 'utf8');
const script = source.match(/\.initialization_script\(\s*r#"([\s\S]*?)"#/);
assert.ok(script, 'the dashboard initialization script must be exercised');

function click(href, nested = false) {
    let listener;
    const opened = [];
    runInNewContext(script[1], {
        document: {
            addEventListener(type, handler, capture) {
                assert.equal(type, 'click');
                assert.equal(capture, true);
                listener = handler;
            },
        },
        window: {
            __TAURI_INTERNALS__: {
                invoke(command, args) { opened.push([command, args.url]); },
            },
        },
    });
    // Anchor URL properties use the same URL parser as a browser. Relative
    // links resolve against the dashboard before the listener sees them.
    const url = href === null ? null : new URL(href, 'http://localhost:5600/');
    const anchor = url && {
        tagName: 'A', href: url.href, protocol: url.protocol, hostname: url.hostname,
    };
    const event = {
        target: nested ? { tagName: 'SPAN', parentElement: anchor } : anchor,
        prevented: false,
        stopped: false,
        preventDefault() { this.prevented = true; },
        stopPropagation() { this.stopped = true; },
    };
    listener(event);
    return { opened, prevented: event.prevented, stopped: event.stopped };
}

for (const href of [
    'https://localhost.example.com/help',
    'https://127.0.0.1.example.com/help',
    'http://localhost@external.example/help',
    'http://127.0.0.1@external.example/help',
    'http://localhost:password@external.example/help',
    'http://127.0.0.1:password@external.example/help',
    'https://activitywatch.net/',
    '//external.example/help',
    'mailto:support@example.com',
]) {
    test(`opens external link in the browser: ${href}`, () => {
        assert.deepEqual(click(href, true), {
            opened: [['open_external', new URL(href, 'http://localhost:5600/').href]],
            prevented: true,
            stopped: true,
        });
    });
}

for (const href of [
    'http://localhost',
    'https://localhost:5600/#/activity',
    'http://LOCALHOST:5600/#/activity',
    'http://127.0.0.1:5600/#/activity',
    'https://127.0.0.1/',
    '/#/settings',
    '#/activity',
    'tauri://localhost/',
    'about:blank',
]) {
    test(`keeps local navigation in the dashboard: ${href}`, () => {
        assert.deepEqual(click(href), { opened: [], prevented: false, stopped: false });
    });
}

test('ignores clicks outside links', () => {
    assert.deepEqual(click(null, true), { opened: [], prevented: false, stopped: false });
});
