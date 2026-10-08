'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { test } = require('node:test');

// Fixtures are inert text files. No marker is executed and no download occurs.
function fixture(t) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'mcptracer-discovery-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const packageRoot = path.join(root, 'child', 'node_modules', 'mcptracer');
  fs.mkdirSync(path.join(packageRoot, 'bin'), { recursive: true });
  fs.copyFileSync(path.join(__dirname, '../bin/mcptracer.js'), path.join(packageRoot, 'bin/mcptracer.js'));
  fs.copyFileSync(path.join(__dirname, '../package.json'), path.join(packageRoot, 'package.json'));
  const wrapper = require(path.join(packageRoot, 'bin/mcptracer.js'));
  t.mock.method(os, 'homedir', () => path.join(root, 'home'));
  const original = process.env.MCPTRACER_BIN;
  delete process.env.MCPTRACER_BIN;
  t.after(() => {
    if (original === undefined) delete process.env.MCPTRACER_BIN;
    else process.env.MCPTRACER_BIN = original;
  });
  return { root, wrapper };
}

test('ambient Cargo markers cannot select a development binary', (t) => {
  const { root, wrapper } = fixture(t);
  fs.writeFileSync(path.join(root, 'Cargo.toml'), '[workspace]\n# crates/mcptracer-proxy\n');
  for (const profile of ['release', 'debug']) {
    const directory = path.join(root, 'target', profile);
    fs.mkdirSync(directory, { recursive: true });
    for (const exe of ['mcptracer', 'mcptracer.exe']) {
      fs.writeFileSync(path.join(directory, exe), 'dummy, not executable');
    }
  }
  assert.equal(wrapper.findBinary(), null);
});

test('explicit binary selection is preserved', (t) => {
  const { root, wrapper } = fixture(t);
  const chosen = path.join(root, 'chosen-binary');
  fs.writeFileSync(chosen, 'dummy, not executable');
  process.env.MCPTRACER_BIN = chosen;
  assert.equal(wrapper.findBinary(), chosen);
});

test('invalid explicit selection fails closed', (t) => {
  const { root, wrapper } = fixture(t);
  for (const value of ['', path.join(root, 'missing'), root]) {
    process.env.MCPTRACER_BIN = value;
    assert.throws(() => wrapper.findBinary(), /existing binary file/);
  }
});
