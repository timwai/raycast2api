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

function mount(tag = 'proxy-panel') {
    const elements = new Map(); const notices = [];
    const scope = {
        LitElement: class { requestUpdate() {} dispatchEvent(event) { this.lastEvent = event; } }, html: (parts, ...values) => ({ parts, values }),
        customElements: { define: (name, element) => elements.set(name, element) },
        message: (...args) => notices.push(args), formatDate: () => '', setTimeout, Date, structuredClone,
        CustomEvent: class { constructor(type, options) { this.type = type; Object.assign(this, options); } }
    };
    runInNewContext(readFileSync(join(__dirname, '../src/page/views/proxy.js'), 'utf8')
        .replace(/^import .*;\r?\n/gm, '').replace(/^export /gm, '') + '\nthis.parse = parseBatchSubscriptions;', scope);
    const panel = new (elements.get(tag))();
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

function templates(value) {
    if (Array.isArray(value)) return Array.from(value).flatMap(templates);
    if (!value?.parts) return [];
    return [value, ...Array.from(value.values).flatMap(templates)];
}

test('all-node testing sends one unfiltered request and refreshes account test status', async () => {
    const { panel } = mount();
    panel.keyword = 'filter'; panel.subscription = 'sub'; panel.page = 5;
    panel.request = async (path, body) => {
        assert.equal(path, 'nodes/test-all');
        assert.equal(Object.keys(body).length, 0);
        return { id: 'all', phase: 'committed', result: { total: 35, succeeded: 34, items: [] } };
    };
    await panel.testAll();
    assert.equal(panel.results.total, 35);
    assert.equal(panel.lastEvent.type, 'proxy-tests-updated');
});

test('binding options disable untested and failed nodes and reject a forged selection', async () => {
    const { panel: dialog } = mount('proxy-binding-dialog');
    dialog.account = { email: 'account@test', proxy_binding: null };
    dialog.nodes = [
        { id: 'passing', original_name: 'passing', protocol: 'http', presence: 'present', subscription_enabled: true, test_status: 'passed', delay_ms: 42 },
        { id: 'failed', original_name: 'failed', protocol: 'http', presence: 'present', subscription_enabled: true, test_status: 'failed' },
        { id: 'untested', original_name: 'untested', protocol: 'http', presence: 'present', subscription_enabled: true, test_status: 'untested' }
    ];
    const options = templates(dialog.render()).filter(t => t.parts[0] === '<option value=');
    assert.equal(options.length, 3);
    assert.equal(options[0].values[1], false);
    assert.equal(options[1].values[1], true);
    assert.equal(options[2].values[1], true);
    let calls = 0; dialog.request = async () => { calls++; };
    dialog.selected = 'failed';
    await dialog.save({ preventDefault() {} });
    assert.equal(calls, 0);
    assert.match(dialog.error, /尚未通过测试/);
});

test('a failed existing binding still allows explicit unbinding', async () => {
    const { panel: dialog } = mount('proxy-binding-dialog');
    dialog.account = { email: 'account@test', proxy_binding: { node_id: 'failed', revision: 7, test_status: 'failed' } };
    dialog.selected = ''; dialog.close = () => {};
    dialog.request = async (path, body) => {
        assert.equal(path, 'bindings/delete'); assert.equal(body.expected_revision, 7);
        return { id: 'unbind', phase: 'committed' };
    };
    await dialog.save({ preventDefault() {} });
    assert.equal(dialog.lastEvent.type, 'binding-updated');
});

function renderedText(value) {
    if (Array.isArray(value)) return Array.from(value).map(renderedText).join('');
    if (value?.parts) return value.parts.map((part,index)=>part+renderedText(value.values[index])).join('');
    return value == null || typeof value === 'function' || typeof value === 'object' ? '' : String(value);
}
test('exit choices identify bound accounts and unused nodes', () => {
    const {panel:dialog}=mount('proxy-binding-dialog');
    dialog.nodes=[{id:'a',original_name:'node-a',subscription_id:'sub',presence:'present',subscription_enabled:true,test_status:'passed',bound_accounts:['one@fixture','two@fixture'],delay_ms:15},
        {id:'b',original_name:'node-b',subscription_id:'sub',presence:'present',subscription_enabled:true,test_status:'passed',binding_count:0,delay_ms:25}];
    const text=renderedText(dialog.render());
    assert.match(text,/已绑定：one@fixture、two@fixture/);assert.match(text,/未绑定/);
});
test('proxy settings save all fields with their revision and display the saved values',async()=>{
    const {panel,notices}=mount();let saved=null;
    panel.request=async(path,body)=>{
        if(path==='settings') return {revision:saved?5:4,settings:saved||{auto_test_enabled:true,auto_test_interval_secs:1800,manual_test_concurrency:3}};
        assert.equal(path,'settings/update');assert.equal(body.expected_revision,4);saved=body.settings;
        return {id:'settings',phase:'committed'};
    };
    await panel.toggleSettings();assert.equal(panel.showSettings,true);
    panel.setSetting('manual_test_concurrency',6);
    await panel.saveSettings({preventDefault(){}});
    assert.equal(saved.manual_test_concurrency,6);assert.equal(saved.auto_test_interval_secs,1800);
    assert.equal(panel.preferences.revision,5);assert.equal(panel.settingsBusy,false);
    assert.ok(notices.some(([text])=>text.includes('已保存并生效')));
});
test('saving the global strategy explicitly cancels a pending automatic assignment',async()=>{
    const {panel:dialog}=mount('proxy-binding-dialog');
    dialog.account={email:'pending@fixture',proxy_binding:{status:'pending',node_id:null,revision:0}};
    dialog.selected='';dialog.close=()=>{};let called=false;
    dialog.request=async(path,body)=>{assert.equal(path,'bindings/delete');assert.equal(body.expected_revision,0);called=true;return {id:'cancel',phase:'committed'};};
    await dialog.save({preventDefault(){}});assert.equal(called,true);
});
