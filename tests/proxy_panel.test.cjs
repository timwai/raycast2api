const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const { test } = require('node:test');
const { runInNewContext } = require('node:vm');
const { randomUUID } = require('node:crypto');

function requests() {
    const scope = { crypto: { randomUUID } };
    runInNewContext(readFileSync(join(__dirname, '../src/page/proxy-request.js'), 'utf8').replace('export class', 'class') + '\nthis.Client = ProxyRequests;', scope);
    return new scope.Client();
}

test('overlapping requests coalesce and lost responses reuse the key, then accepted repeats get a new key', async () => {
    const client = requests();
    const keys = [];
    const send = async headers => { keys.push(headers['Idempotency-Key']); throw new Error('connection lost'); };
    const first = client.run('proxy', 'nodes/test', { id: 'node' }, send);
    const duplicate = client.run('proxy', 'nodes/test', { id: 'node' }, send);
    assert.equal(first, duplicate);
    await assert.rejects(first);
    await client.run('proxy', 'nodes/test', { id: 'node' }, async headers => keys.push(headers['Idempotency-Key']));
    await client.run('proxy', 'nodes/test', { id: 'node' }, async headers => keys.push(headers['Idempotency-Key']));
    assert.equal(keys[0], keys[1]);
    assert.notEqual(keys[1], keys[2]);
});

test('body order is canonical, scopes stay separate and clearing a session discards retry keys', async () => {
    const client = requests();
    let first;
    await assert.rejects(client.run('proxy', 'subscriptions/update', { id: 'a', enabled: true }, async h => { first = h['Idempotency-Key']; throw Error('lost'); }));
    await assert.rejects(client.run('proxy', 'subscriptions/update', { enabled: true, id: 'a' }, async h => { assert.equal(h['Idempotency-Key'], first); throw Error('lost'); }));
    client.clear();
    await client.run('proxy', 'subscriptions/update', { id: 'a', enabled: true }, async h => assert.notEqual(h['Idempotency-Key'], first));
    await client.run('proxy', 'nodes/list', {}, async h => assert.equal(Object.keys(h).length, 0));
    await client.run('accounts', 'delete', {}, async h => assert.equal(Object.keys(h).length, 0));
});

test('definitive validation errors release their idempotency key', async () => {
    const client = requests(); let first;
    await assert.rejects(client.run('proxy', 'subscriptions/create-batch', { items: [] }, async h => {
        first = h['Idempotency-Key']; throw Object.assign(Error('invalid'), { retryable: false });
    }));
    await client.run('proxy', 'subscriptions/create-batch', { items: [] }, async h => assert.notEqual(h['Idempotency-Key'], first));
});

function mount() {
    const elements = new Map(); const notices = [];
    const scope = {
        LitElement: class { requestUpdate() {} }, html: (parts, ...values) => ({ parts, values }),
        customElements: { define: (name, element) => elements.set(name, element) },
        message: (...args) => notices.push(args), formatDate: () => '', setTimeout, Date
    };
    runInNewContext(readFileSync(join(__dirname, '../src/page/views/proxy.js'), 'utf8')
        .replace(/^import .*;\r?\n/gm, '').replace(/^export /gm, '') + '\nthis.parse = parseBatchSubscriptions;', scope);
    const panel = new (elements.get('proxy-panel'))();
    panel.load = async () => {};
    return { panel, notices, parse: scope.parse };
}

test('batch addition parses optional names, ignores blank lines and limits input to 50', () => {
    const { parse } = mount();
    const items = parse('https://one.invalid/sub\n\n测试 | https://two.invalid/sub\n', 300);
    assert.equal(items.length, 2);
    assert.equal(items[0].url, 'https://one.invalid/sub');
    assert.equal(items[1].name, '测试');
    assert.equal(items[1].refresh_interval_secs, 300);
    assert.throws(() => parse('', 300));
    assert.throws(() => parse(Array(51).fill('https://test.invalid').join('\n'), 300));
});

test('batch selection caps at 20 and partial failures remain visible after completion', async () => {
    const { panel, notices } = mount();
    for (let i = 0; i < 21; i++) panel.toggleNode({ id: String(i) }, true);
    assert.equal(panel.selected.size, 20);
    panel.request = async (path, body) => {
        assert.equal(path, 'nodes/test-batch'); assert.equal(body.ids.length, 20);
        return { id: 'batch', phase: 'committed', result: { total: 20, succeeded: 19, items: [{ success: false, code: 'proxy_test_failed' }] } };
    };
    await panel.testSelected();
    assert.equal(panel.selected.size, 0);
    assert.equal(panel.results.succeeded, 19);
    assert.equal(panel.results.items[0].code, 'proxy_test_failed');
    assert.ok(notices.some(([text]) => text.includes('失败 1 项')));
});

test('single node testing waits for operation completion and surfaces failed tests', async () => {
    const { panel, notices } = mount(); let calls = 0;
    panel.request = async path => {
        calls++;
        if (path === 'nodes/test') return { id: 'test', phase: 'queued' };
        assert.equal(path, 'operations/detail');
        return { id: 'test', phase: 'committed', result: { success: false, code: 'proxy_test_failed' } };
    };
    await panel.test({ id: 'a' });
    assert.equal(calls, 2);
    assert.match(panel.error, /proxy_test_failed/);
    assert.ok(!notices.some(([text]) => text.includes('延迟：')));
});
