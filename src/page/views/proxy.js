import { LitElement, html } from 'https://cdn.jsdelivr.net/gh/lit/dist@3/core/lit-core.min.js';
import { message } from '../components.js';
import { formatDate } from '../utils.js';

export async function waitProxyOperation(request, operation, progress = () => {}) {
    const deadline = Date.now() + 600000;
    while (Date.now() < deadline) {
        progress(operation.phase, operation);
        if (operation.phase === 'committed') return operation;
        if (['failed', 'rolled_back', 'recovery_required'].includes(operation.phase))
            throw new Error(operation.message || operation.code || '代理配置应用失败');
        await new Promise(resolve => setTimeout(resolve, 700));
        operation = await request('operations/detail', { id: operation.id });
    }
    throw new Error('操作仍在排队或恢复中，请刷新状态查看结果。');
}

export function parseBatchSubscriptions(text, interval) {
    const lines = text.split(/\r?\n/).map(line => line.trim()).filter(Boolean);
    if (!lines.length || lines.length > 50) throw new Error('每批请输入 1–50 个订阅，每行一个。');
    return lines.map(line => {
        const separator = line.includes('\t') ? '\t' : ' | ';
        const index = line.indexOf(separator);
        return { ...(index < 0 ? {} : { name: line.slice(0, index).trim() }),
            url: (index < 0 ? line : line.slice(index + separator.length)).trim(),
            refresh_interval_secs: Number(interval) };
    });
}

class ProxyBindingDialog extends LitElement {
    static properties = {
        request: { attribute: false }, account: { state: true }, nodes: { state: true },
        selected: { state: true }, busy: { state: true }, error: { state: true }, keyword: { state: true }
    };
    constructor() { super(); Object.assign(this, { account: null, nodes: [], selected: '', busy: false, error: '', keyword: '' }); }
    createRenderRoot() { return this; }
    updated() { const select = this.querySelector("select"); if (select && select.value !== this.selected) select.value = this.selected; }
    async open(email) {
        this.error = ''; this.busy = true; this.keyword = '';
        try {
            this.account = await this.request('detail', { email }, 'accounts');
            this.selected = this.account.proxy_binding?.node_id || '';
            await this.search();
            await this.updateComplete;
            this.querySelector('dialog')?.showModal();
        } catch (error) { message(error.message); } finally { this.busy = false; }
    }
    async search(event) {
        event?.preventDefault();
        try {
            const data = await this.request('nodes/list', { page: 1, page_size: 200, keyword: this.keyword, status: 'present' });
            this.nodes = data.items || [];
        } catch (error) { this.error = error.message; }
    }
    close() { if (!this.busy) { this.querySelector('dialog')?.close(); this.account = null; } }
    async save(event) {
        event.preventDefault(); if (this.busy) return;
        const binding = this.account.proxy_binding;
        if (this.selected === (binding?.node_id || '')) return this.close();
        this.busy = true; this.error = '';
        try {
            const path = this.selected ? 'bindings/set' : 'bindings/delete';
            const operation = await this.request(path, { email: this.account.email, ...(this.selected ? { node_id: this.selected } : {}), expected_revision: binding?.revision || 0 });
            await waitProxyOperation(this.request, operation);
            this.busy = false; this.close();
            this.dispatchEvent(new CustomEvent('binding-updated', { bubbles: true, composed: true }));
            message('账号代理出口已更新', 'success');
        } catch (error) { this.error = error.message; } finally { this.busy = false; }
    }
    render() {
        const groups = new Map();
        for (const node of this.nodes) {
            const label = node.subscription_name || node.subscription_id;
            if (!groups.has(label)) groups.set(label, []);
            groups.get(label).push(node);
        }
        const current = this.account?.proxy_binding;
        return html`<dialog style="margin:auto;max-height:90dvh;overflow:auto" class="w-full max-w-xl rounded-xl border border-line p-6 shadow-xl backdrop:bg-black/30" @cancel=${event => { if (this.busy) event.preventDefault(); }}>
            <h2 class="mb-2 font-semibold">账号代理出口</h2>
            <p class="mb-4 break-all text-sm text-muted">${this.account?.email || ''}</p>
            <form class="mb-3 flex gap-2" @submit=${this.search}>
                <input class="min-w-0 flex-1 rounded border border-line px-3 py-2 text-sm" placeholder="搜索节点名称或服务器（最多展示 200 项）" .value=${this.keyword} @input=${e => this.keyword = e.target.value} />
                <button class="rounded border border-line px-3 text-sm" ?disabled=${this.busy}>搜索</button>
            </form>
            <form @submit=${this.save}>
                <label class="block text-sm">出口节点
                    <select class="mt-2 w-full rounded border border-line p-2" .value=${this.selected} @change=${e => this.selected = e.target.value} ?disabled=${this.busy}>
                        <option value="">不绑定，使用现有全局策略</option>
                        ${current && !this.nodes.some(n => n.id === current.node_id) ? html`<option value=${current.node_id}>当前：${current.node_name || current.node_id}（${current.status}）</option>` : ''}
                        ${[...groups].map(([label, nodes]) => html`<optgroup label=${label}>${nodes.map(n => html`<option value=${n.id} ?disabled=${!n.subscription_enabled}>${n.original_name} · ${n.protocol}${n.subscription_enabled ? '' : '（订阅停用）'}</option>`)}</optgroup>`)}
                    </select>
                </label>
                <p class="mt-3 text-xs text-muted">配置应用可能中断在途请求。绑定节点不可用时拒绝请求；只有显式解绑才恢复全局策略。</p>
                ${this.error ? html`<p role="alert" class="mt-3 text-sm text-red-700">${this.error}</p>` : ''}
                <div class="mt-5 flex justify-end gap-3">
                    <button type="button" class="rounded border border-line px-4 py-2 text-sm" @click=${this.close} ?disabled=${this.busy}>取消</button>
                    <button class="rounded bg-blue-600 px-4 py-2 text-sm text-white" ?disabled=${this.busy}>${this.busy ? '正在应用…' : '保存出口'}</button>
                </div>
            </form>
        </dialog>`;
    }
}
customElements.define('proxy-binding-dialog', ProxyBindingDialog);

class ProxyPanel extends LitElement {
    static properties = {
        request: { attribute: false }, subscriptions: { state: true }, nodes: { state: true },
        runtime: { state: true }, loading: { state: true }, busy: { state: true }, error: { state: true },
        keyword: { state: true }, subscription: { state: true }, country: { state: true }, presence: { state: true }, page: { state: true },
        pages: { state: true }, total: { state: true }, sortDelay: { state: true }, editor: { state: true }, name: { state: true }, url: { state: true }, interval: { state: true }, batchText: { state: true }, selected: { state: true }, progress: { state: true }, results: { state: true }
    };
    constructor() {
        super(); Object.assign(this, { subscriptions: [], nodes: [], runtime: null, loading: false, busy: false, error: '', keyword: '', subscription: '', country: '', presence: 'present', page: 1, pages: 0, total: 0, sortDelay: false, editor: null, name: '', url: '', interval: 21600, batchText: '', selected: new Set(), progress: '', results: null });
    }
    createRenderRoot() { return this; }
    async allSubscriptions() {
        const items = []; let page = 1;
        for (;;) {
            const result = await this.request('subscriptions/list', { page, page_size: 200 });
            items.push(...result.items);
            if (items.length >= result.total || !result.items.length) return items;
            page++;
        }
    }
    async load() {
        if (this.loading) return; this.loading = true; this.error = '';
        try {
            const [subscriptions, nodes, runtime] = await Promise.all([
                this.allSubscriptions(),
                this.request('nodes/list', { page: this.page, page_size: 50, keyword: this.keyword, subscription_id: this.subscription || undefined, country: this.country || undefined, status: this.presence, sort_by_delay: this.sortDelay }),
                this.request('status', {})
            ]);
            this.subscriptions = subscriptions; this.nodes = nodes.items; this.pages = nodes.pages; this.total = nodes.total; this.runtime = runtime;
        } catch (error) { this.error = error.message; } finally { this.loading = false; }
    }
    async change(path, body, success = '配置已生效') {
        if (this.busy) return; this.busy = true; this.error = ''; this.results = null; this.progress = '任务已排队…';
        try {
            const operation = await this.request(path, body);
            const done = await waitProxyOperation(this.request, operation, (phase, current) => {
                const r = current.result;
                this.progress = r?.total != null ? `已完成 ${r.completed ?? r.items?.length ?? 0} / ${r.total}` : `任务状态：${phase}`;
            });
            if (done.result?.items) {
                this.results = done.result;
                const r = done.result;
                message(`完成 ${r.total} 项，成功 ${r.succeeded} 项，失败 ${r.total - r.succeeded} 项`, r.succeeded === r.total ? 'success' : 'warning');
            } else if (success) message(success, 'success');
            this.editor = null; this.url = ''; this.batchText = ''; await this.load();
            return done;
        } catch (error) { this.error = error.message; } finally { this.busy = false; }
    }
    edit(sub = null) { this.results = null; this.editor = sub || {}; this.name = sub?.name || ''; this.url = ''; this.interval = sub?.refresh_interval_secs || 21600; }
    async save(event) {
        event.preventDefault();
        const sub = this.editor;
        if (sub.batch) {
            try { await this.change('subscriptions/create-batch', { items: parseBatchSubscriptions(this.batchText, this.interval) }); }
            catch (error) { this.error = error.message; }
            return;
        }
        await this.change(sub.id ? 'subscriptions/update' : 'subscriptions/create', {
            ...(sub.id ? { id: sub.id, expected_revision: sub.revision } : {}), name: this.name.trim(), url: this.url,
            refresh_interval_secs: Number(this.interval)
        }, sub.id ? '订阅已更新' : '订阅已添加，初次导入已排队');
    }
    async test(node) {
        const operation = await this.change('nodes/test', { id: node.id }, '');
        if (operation?.result?.success) message('延迟：' + operation.result.delay + ' ms', 'success');
        else if (operation) this.error = '测速失败：' + (operation.result?.code || '节点不可达');
    }
    toggleNode(node, checked) {
        if (checked && this.selected.size >= 20) { message('每批最多选择 20 个节点'); this.requestUpdate(); return; }
        const selected = new Set(this.selected);
        checked ? selected.add(node.id) : selected.delete(node.id);
        this.selected = selected;
    }
    async testSelected() {
        if (!this.selected.size) return;
        if (await this.change('nodes/test-batch', { ids: [...this.selected] })) this.selected = new Set();
    }
    render() {
        const button = 'rounded border border-line bg-white px-3 py-1.5 text-xs disabled:opacity-50';
        return html`<section class="h-full overflow-auto p-4" aria-label="代理管理">
            <div class="mb-4 flex flex-wrap items-center justify-between gap-3">
                <div><h1 class="font-semibold">代理管理</h1><p class="mt-1 text-xs text-muted">${this.runtime?.enabled ? this.runtime.ready ? '核心就绪' : '核心不可用，绑定账号已阻断' : '模块未启用'} · 配置代次 ${this.runtime?.generation ?? '—'} · ${this.runtime?.bindings || 0} 个账号绑定</p></div>
                <div class="flex flex-wrap gap-2"><button class=${button} @click=${() => this.change('subscriptions/refresh-all', {})} ?disabled=${this.busy || !this.runtime?.enabled || !this.subscriptions.some(s => s.enabled)}>刷新全部</button><button class=${button} @click=${() => { this.edit(); this.editor = { batch: true }; }} ?disabled=${this.busy || !this.runtime?.enabled}>批量添加</button><button class=${button} @click=${() => this.edit()} ?disabled=${this.busy || !this.runtime?.enabled}>添加订阅</button><button class=${button} @click=${this.load} ?disabled=${this.loading}>刷新页面</button></div>
            </div>
            ${this.error ? html`<p role="alert" class="mb-3 rounded bg-red-50 p-3 text-sm text-red-700">${this.error}</p>` : ''}
            ${this.busy ? html`<p role="status" class="mb-3 text-sm text-blue-700">${this.progress}；配置更新可能中断在途请求。</p>` : ''}
            ${this.results ? html`<div aria-label="批量操作结果" class="mb-4 rounded border border-line p-3 text-xs"><p>共 ${this.results.total} 项，成功 ${this.results.succeeded} 项，失败 ${this.results.total - this.results.succeeded} 项</p><ul class="mt-2 max-h-48 overflow-auto">${this.results.items.map((item, index) => html`<li class="py-1">${item.name || this.nodes.find(n => n.id === item.target_id)?.original_name || this.subscriptions.find(s => s.id === item.target_id)?.name || `第 ${index + 1} 项`}：${item.success ? item.result?.delay ? `${item.result.delay} ms` : '成功' : item.message || item.code || item.result?.code || '失败'}</li>`)}</ul></div>` : ''}
            ${this.editor ? html`<form class="mb-5 grid gap-3 rounded-lg border border-line bg-white p-4 sm:grid-cols-2" @submit=${this.save}>
                ${this.editor.batch ? html`<label class="text-xs">批量订阅（每行一个，最多 50 个）<textarea required rows="5" spellcheck="false" autocomplete="off" class="mt-1 block w-full rounded border border-line p-2 text-sm" placeholder="https://example.com/subscription&#10;名称 | https://example.com/another" .value=${this.batchText} @input=${e => this.batchText = e.target.value}></textarea></label>` : html`<label class="text-xs">名称<input required maxlength="256" class="mt-1 block w-full rounded border border-line p-2 text-sm" .value=${this.name} @input=${e => this.name = e.target.value} /></label>`}
                <label class="text-xs">刷新间隔（秒）<input type="number" min="300" required class="mt-1 block w-full rounded border border-line p-2 text-sm" .value=${String(this.interval)} @input=${e => this.interval = e.target.value} /></label>
                ${!this.editor.batch ? html`<label class="text-xs sm:col-span-2">HTTPS 订阅地址${this.editor.id ? '（留空保持原地址）' : ''}<input type="password" autocomplete="new-password" ?required=${!this.editor.id} class="mt-1 block w-full rounded border border-line p-2 text-sm" .value=${this.url} @input=${e => this.url = e.target.value} /></label>` : ''}
                <div class="flex gap-2 sm:col-span-2"><button class=${button} ?disabled=${this.busy}>保存</button><button type="button" class=${button} @click=${() => { this.editor = null; this.url = ''; this.batchText = ''; }} ?disabled=${this.busy}>取消</button></div>
            </form>` : ''}
            <div class="mb-5 overflow-x-auto rounded-lg border border-line bg-white"><table style="min-width:760px" class="w-full text-left text-xs"><caption class="p-3 text-left font-semibold">订阅</caption><thead class="border-y border-line bg-slate-50"><tr>${['名称', '状态', '节点', '最近成功', '最近错误', '操作'].map(x => html`<th class="p-3">${x}</th>`)}</tr></thead>
                <tbody>${this.subscriptions.map(s => html`<tr class="border-b border-line"><td class="p-3">${s.name}</td><td class="p-3">${s.enabled ? '启用' : '停用'}</td><td class="p-3">${s.node_count}</td><td class="p-3">${s.last_success_at ? formatDate(s.last_success_at) : '尚未刷新'}</td><td class="p-3">${s.last_error_code || '—'}</td><td class="p-3"><div class="flex gap-2">
                    <button class=${button} ?disabled=${this.busy || !s.enabled} @click=${() => this.change('subscriptions/refresh', { id: s.id }, '订阅刷新完成')}>刷新</button>
                    <button class=${button} ?disabled=${this.busy} @click=${() => this.edit(s)}>编辑</button>
                    <button class=${button} ?disabled=${this.busy} @click=${() => this.change('subscriptions/update', { id: s.id, expected_revision: s.revision, enabled: !s.enabled })}>${s.enabled ? '停用' : '启用'}</button>
                    <button class=${button} ?disabled=${this.busy} @click=${() => { if (confirm('删除订阅“' + s.name + '”？仍有账号绑定时会拒绝删除。')) this.change('subscriptions/delete', { id: s.id, expected_revision: s.revision }); }}>删除</button>
                </div></td></tr>`)}</tbody></table>${!this.subscriptions.length ? html`<p class="p-4 text-sm text-muted">暂无订阅。启用模块后添加 HTTPS Clash 订阅，再手动刷新导入。</p>` : ''}</div>
            <form class="mb-3 flex flex-wrap gap-2" @submit=${event => { event.preventDefault(); this.page = 1; this.load(); }}>
                <input class="rounded border border-line px-3 py-2 text-sm" placeholder="节点名称或服务器" .value=${this.keyword} @input=${e => this.keyword = e.target.value} />
                <select class="rounded border border-line px-2 text-sm" .value=${this.subscription} @change=${e => this.subscription = e.target.value}><option value="">全部订阅</option>${this.subscriptions.map(s => html`<option value=${s.id}>${s.name}</option>`)}</select>
                <select aria-label="节点状态" class="rounded border border-line px-2 text-sm" .value=${this.presence} @change=${e => { this.presence = e.target.value; this.page = 1; this.load(); }}>
                    <option value="present">当前节点</option><option value="missing">失效绑定节点</option><option value="all">全部节点</option>
                </select>
                <input class="w-32 rounded border border-line px-3 py-2 text-sm" placeholder="地区标签" .value=${this.country} @input=${e => this.country = e.target.value} />
                <label class="flex items-center gap-1 text-xs"><input type="checkbox" .checked=${this.sortDelay} @change=${e => this.sortDelay = e.target.checked} />按延迟排序</label><button class=${button}>搜索</button>
            </form>
            <div class="mb-3 flex flex-wrap items-center gap-2"><button class=${button} ?disabled=${this.busy || !this.selected.size || !this.runtime?.ready} @click=${this.testSelected}>批量测速（${this.selected.size}/20）</button><button class=${button} ?disabled=${this.busy || !this.selected.size} @click=${() => this.selected = new Set()}>清空选择</button><span class="text-xs text-muted">测速仅在手动操作时运行</span></div>
            <div class="overflow-x-auto rounded-lg border border-line bg-white"><table style="min-width:760px" class="w-full text-left text-xs"><caption class="p-3 text-left font-semibold">节点 · 共 ${this.total} 项</caption><thead class="border-y border-line bg-slate-50"><tr><th class="p-3">选择</th>${['节点 / 订阅', '协议 / 服务器', '地区', '状态', '延迟', '绑定数', '操作'].map(x => html`<th class="p-3">${x}</th>`)}</tr></thead><tbody>${this.nodes.map(n => html`<tr class="border-b border-line"><td class="p-3"><input type="checkbox" aria-label=${'选择节点 ' + n.original_name} .checked=${this.selected.has(n.id)} ?disabled=${this.busy || n.presence !== 'present' || !n.subscription_enabled || (!this.selected.has(n.id) && this.selected.size >= 20)} @change=${e => this.toggleNode(n, e.target.checked)} /></td><td class="p-3">${n.original_name}<p class="mt-1 text-muted">${n.subscription_name}</p></td><td class="p-3">${n.protocol}<p class="mt-1 text-muted">${n.server}:${n.server_port}</p></td><td class="p-3">${n.country || '未知'}</td><td class="p-3">${n.presence === 'present' ? n.subscription_enabled ? '已导入' : '订阅停用' : '已缺失'}</td><td class="p-3">${n.delay_ms == null ? '未测试' : n.delay_ms + ' ms'}</td><td class="p-3">${n.binding_count}</td><td class="p-3"><button class=${button} ?disabled=${this.busy || n.presence !== 'present' || !n.subscription_enabled || !this.runtime?.ready} @click=${() => this.test(n)}>手动测速</button></td></tr>`)}</tbody></table></div>
            <div class="mt-3 flex items-center justify-end gap-3 text-xs"><button class=${button} ?disabled=${this.page <= 1 || this.loading} @click=${() => { this.page--; this.load(); }}>上一页</button><span>${this.page} / ${this.pages || 1}</span><button class=${button} ?disabled=${this.page >= this.pages || this.loading} @click=${() => { this.page++; this.load(); }}>下一页</button></div>
        </section>`;
    }
}
customElements.define('proxy-panel', ProxyPanel);
