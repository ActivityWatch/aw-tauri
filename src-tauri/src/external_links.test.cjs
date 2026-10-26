// Exercise the actual initialization script with browser-like anchor clicks.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const source = fs.readFileSync(__dirname + '/lib.rs', 'utf8');
const script = source.match(/\.initialization_script\(\s*r#"([\s\S]*?)"#/)[1];
let listener;
const calls = [];
vm.runInNewContext(script, {
  document: { addEventListener: (name, fn) => { assert.equal(name, 'click'); listener = fn; } },
  window: { __TAURI_INTERNALS__: { invoke: (command, args) => calls.push({ command, args }) } },
});
function click(href, download) {
  calls.length = 0;
  let prevented = false;
  const url = new URL(href, 'http://localhost:5600/');
  listener({ target: { tagName: 'A', href: url.href, protocol: url.protocol, hostname: url.hostname, hasAttribute: name => name === 'download' && download !== undefined }, preventDefault() { prevented = true; }, stopPropagation() {} });
  return { prevented, calls: calls.length };
}
assert.deepEqual(click('blob:http://localhost:5600/synthetic', 'aw-bucket-export.json'), { prevented: false, calls: 0 });
assert.deepEqual(click('blob:http://localhost:5600/plain'), { prevented: true, calls: 1 });
assert.deepEqual(click('http://localhost:5600/#/buckets'), { prevented: false, calls: 0 });
assert.deepEqual(click('https://activitywatch.net/'), { prevented: true, calls: 1 });
assert.deepEqual(click('mailto:test@example.org'), { prevented: true, calls: 1 });
console.log('PASS blob export bypasses external opener; local and external links retain behavior');
