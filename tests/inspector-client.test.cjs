'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const { test } = require('node:test');

function client() {
  const stored = new Map();
  const alerts = [];
  const node = () => {
    const classes = new Set();
    return {
      children: [], attributes: {}, listeners: {}, textContent: '',
      appendChild(child) { this.children.push(child); },
      replaceChildren(...children) { this.children = children; },
      setAttribute(key, value) { this.attributes[key] = value; },
      addEventListener(name, callback) { this.listeners[name] = callback; },
      classList: {
        toggle(name) { if (classes.has(name)) { classes.delete(name); return false; } classes.add(name); return true; },
        add(name) { classes.add(name); }, remove(name) { classes.delete(name); },
      },
    };
  };
  const context = vm.createContext({
    URLSearchParams,
    document: { getElementById: node, createElement: node, createTextNode: (text) => ({ textContent: text }) },
    window: {
      location: { hash: '#token=dummy-token', pathname: '/', search: '' },
      history: { replaceState() {} },
      sessionStorage: {
        setItem: (key, value) => stored.set(key, value),
        getItem: (key) => stored.get(key) ?? null,
        removeItem: (key) => stored.delete(key),
      },
      addEventListener() {},
      confirm() { throw new Error('401 must not prompt for export consent'); },
    },
    fetch: async () => ({ status: 200, ok: true, json: async () => [] }),
    alert: (message) => alerts.push(message),
  });
  const source = fs.readFileSync(
    path.join(__dirname, '../crates/mcptracer-proxy/assets/inspect/app.js'), 'utf8'
  );
  vm.runInContext(source, context, { timeout: 1000 });
  const api = vm.runInContext(
    '({ fetchJson, downloadExport, buildExchangeIndex, exchangeFor, renderMessageRow, renderCaptureHealth, token: () => accessToken })', context,
    { timeout: 1000 }
  );
  return { context, api, stored, alerts };
}

test('API 401 clears memory and session storage and prevents token reuse', async () => {
  const { context, api, stored } = client();
  let requests = 0;
  context.fetch = async () => { requests++; return { status: 401, ok: false }; };
  await assert.rejects(api.fetchJson('/api/sessions'), /access expired/);
  assert.equal(api.token(), null);
  assert.equal(stored.size, 0);
  await assert.rejects(api.fetchJson('/api/sessions'), /Open the inspector link/);
  assert.equal(requests, 1);
});

test('export 401 expires the same token without consent retries', async () => {
  const { context, api, stored, alerts } = client();
  let requests = 0;
  context.fetch = async () => { requests++; return { status: 401, ok: false }; };
  await api.downloadExport('dummy-session');
  assert.equal(api.token(), null);
  assert.equal(stored.size, 0);
  assert.equal(alerts.length, 1);
  assert.match(alerts[0], /access expired/);
  await api.downloadExport('dummy-session');
  assert.equal(requests, 1);
});

test('exchange indexing visits each exchange once and preserves first-match semantics', () => {
  const { api } = client();
  for (const size of [8, 64, 512]) {
    let visits = 0;
    const exchanges = Array.from({ length: size }, (_, index) => ({
      request_seq: index * 2, response_seq: index * 2 + 1,
      status: index % 2 ? 'error' : 'ok', latency_ns: index,
    }));
    const iterable = {
      *[Symbol.iterator]() {
        for (const exchange of exchanges) { visits++; yield exchange; }
      },
    };
    const index = api.buildExchangeIndex(iterable);
    assert.equal(visits, size);
    for (const exchange of exchanges) {
      assert.equal(api.exchangeFor(index, exchange.request_seq, true), exchange);
      assert.equal(api.exchangeFor(index, exchange.response_seq, false), exchange);
    }
    assert.equal(visits, size, 'lookups must not rescan the exchange iterable');
    assert.equal(api.exchangeFor(index, -1, true), undefined);
    const duplicate = { request_seq: 0, response_seq: 1, status: 'orphan' };
    const repeated = api.buildExchangeIndex([exchanges[0], duplicate]);
    assert.equal(api.exchangeFor(repeated, 0, true), exchanges[0]);
    assert.equal(api.exchangeFor(repeated, 1, false), exchanges[0]);
  }
});

test('message rows preserve indexed status and defer payload formatting until expansion', () => {
  const { api } = client();
  let serializations = 0;
  const payload = { toJSON() { serializations++; return { api_key: 'dummy' }; } };
  const exchange = { request_seq: 0, response_seq: 1, status: 'ok', latency_ns: 1000000 };
  const index = api.buildExchangeIndex([exchange]);
  const rows = api.renderMessageRow({
    seq: 1, message_kind: 'response', direction: 's2c', ts_ns: 0, payload,
  }, index);
  assert.equal(serializations, 0);
  assert.equal(rows[0].children[5].children[0].children[0].textContent, 'ok');
  assert.equal(rows[0].children[6].children[0].textContent, '1.0ms');
  rows[0].listeners.click();
  assert.equal(serializations, 1);
  const panel = rows[1].children[0].children[0];
  assert.match(panel.textContent, /"api_key": "dummy"/);
  rows[0].listeners.click();
  assert.equal(serializations, 1, 'repeat toggles reuse the formatted payload');
});

test('capture-health display fails closed for loss, unfinished or unavailable assessment', () => {
  const { api } = client();
  const text = (node) => node.children.map((child) => child.textContent).join('');
  const session = { dropped_messages: 0, ended_at_ns: 1 };
  const clean = { session, capture_health: { healthy: true, issues: [] } };
  assert.match(text(api.renderCaptureHealth(clean)), /complete and valid/);
  for (const data of [
    { session },
    { ...clean, session: { ...session, ended_at_ns: null } },
    { ...clean, session: { ...session, dropped_messages: 1 } },
    { ...clean, capture_health: { healthy: false, issues: [{ kind: 'invalid_payload', detail: 'dummy-secret-do-not-render' }] } },
  ]) {
    const banner = api.renderCaptureHealth(data);
    assert.equal(banner.attributes.role, 'alert');
    assert.doesNotMatch(text(banner), /complete and valid|dummy-secret-do-not-render/);
  }
});
