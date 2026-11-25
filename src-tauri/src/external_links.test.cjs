// Exercise the actual initialization script with browser-like anchor clicks.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const source = fs.readFileSync(__dirname + '/lib.rs', 'utf8');
const DASHBOARD = 'http://localhost:5600/';
// lib.rs substitutes the dashboard's own port into the script at window-build time.
const script = source
  .match(/\.initialization_script\(\s*&?r#"([\s\S]*?)"#/)[1]
  .replaceAll('__AW_DASHBOARD_PORT__', new URL(DASHBOARD).port || '80');

// The script walks up from the event target looking for an anchor or area.
const documentElement = { tagName: 'HTML', parentElement: null };
let listener;
const calls = [];
vm.runInNewContext(script, {
  document: {
    addEventListener: (name, fn) => {
      assert.equal(name, 'click');
      listener = fn;
    },
    baseURI: DASHBOARD,
    documentElement,
  },
  window: { __TAURI_INTERNALS__: { invoke: (command, args) => calls.push({ command, args }) } },
  // The script resolves hrefs itself; the sandbox needs the browser global.
  URL,
});

// An element exposing attributes the way the DOM does: an HTML <a>/<area> has a
// string href plus parsed protocol/hostname/port properties, while an SVG <a>
// has no such properties and an SVGAnimatedString href.
function element(tagName, attributes, parent) {
  const attrs = attributes || {};
  const raw = attrs.href || attrs['xlink:href'];
  const resolved = raw === undefined ? null : new URL(raw, DASHBOARD);
  const el = {
    tagName,
    parentElement: parent || null,
    getAttribute: (name) => (Object.prototype.hasOwnProperty.call(attrs, name) ? attrs[name] : null),
    hasAttribute: (name) => Object.prototype.hasOwnProperty.call(attrs, name),
  };
  if (tagName === 'A' || tagName === 'AREA') {
    el.href = resolved ? resolved.href : '';
    el.protocol = resolved ? resolved.protocol : '';
    el.hostname = resolved ? resolved.hostname : '';
    el.port = resolved ? resolved.port : '';
  } else if (resolved) {
    el.href = { baseVal: resolved.href };
  }
  return el;
}

function click(target) {
  calls.length = 0;
  let prevented = false;
  listener({ target, preventDefault() { prevented = true; }, stopPropagation() {} });
  return { prevented, calls: calls.length };
}

// HTML anchors
assert.deepEqual(click(element('A', { href: 'blob:http://localhost:5600/synthetic', download: '' })), { prevented: false, calls: 0 });
assert.deepEqual(click(element('A', { href: 'blob:http://localhost:5600/plain' })), { prevented: true, calls: 1 });
assert.deepEqual(click(element('A', { href: 'http://localhost:5600/#/buckets' })), { prevented: false, calls: 0 });
assert.deepEqual(click(element('A', { href: '#/buckets' })), { prevented: false, calls: 0 });
assert.deepEqual(click(element('A', { href: 'https://activitywatch.net/' })), { prevented: true, calls: 1 });
assert.deepEqual(click(element('A', { href: 'mailto:test@example.org' })), { prevented: true, calls: 1 });
assert.deepEqual(click(element('A', { href: 'http://localhost:9999/' })), { prevented: true, calls: 1 });
assert.deepEqual(click(element('A', { href: 'https://localhost.example.com/help' })), { prevented: true, calls: 1 });
assert.deepEqual(click(element('A', { href: 'http://localhost@external.example/help' })), { prevented: true, calls: 1 });
// javascript: pseudo-links must stay in-page; their Vue @click handler must run (aw-tauri#309)
assert.deepEqual(click(element('A', { href: 'javascript:void(0);' })), { prevented: false, calls: 0 });

// aw-tauri#327: SVG <a> is lowercase and its href is an SVGAnimatedString.
assert.deepEqual(click(element('a', { href: 'https://activitywatch.net/' })), { prevented: true, calls: 1 });
assert.deepEqual(click(element('a', { 'xlink:href': 'https://activitywatch.net/' })), { prevented: true, calls: 1 });
// <area href> is a navigable anchor too.
assert.deepEqual(click(element('AREA', { href: 'https://activitywatch.net/' })), { prevented: true, calls: 1 });
// A click on a descendant resolves to the enclosing anchor.
assert.deepEqual(click(element('tspan', {}, element('a', { href: 'https://activitywatch.net/' }))), { prevented: true, calls: 1 });

console.log('PASS blob export bypasses external opener; javascript: stays in-page; SVG/area anchors are intercepted; local and external links retain behavior');
