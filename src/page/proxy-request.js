const mutations = new Set([
    'subscriptions/create', 'subscriptions/create-batch', 'subscriptions/update',
    'subscriptions/delete', 'subscriptions/refresh', 'subscriptions/refresh-all',
    'nodes/test', 'nodes/test-batch', 'nodes/test-all', 'bindings/set', 'bindings/delete'
]);

function canonical(value) {
    if (Array.isArray(value)) return value.map(canonical);
    if (value && typeof value === 'object') return Object.fromEntries(
        Object.keys(value).sort().filter(key => value[key] !== undefined).map(key => [key, canonical(value[key])])
    );
    return value;
}

// A lost response keeps its key for a manual retry; accepted requests can be repeated deliberately.
export class ProxyRequests {
    constructor() { this.pending = new Map(); }
    clear() { this.pending.clear(); }
    run(resource, path, body, send) {
        if (resource !== 'proxy' || !mutations.has(path)) return send({});
        const signature = path + ':' + JSON.stringify(canonical(body));
        let entry = this.pending.get(signature);
        if (entry?.promise) return entry.promise;
        if (!entry) {
            if (this.pending.size >= 256) this.pending.delete(this.pending.keys().next().value);
            entry = { key: crypto.randomUUID() };
            this.pending.set(signature, entry);
        }
        const forget = () => { if (this.pending.get(signature) === entry) this.pending.delete(signature); };
        entry.promise = Promise.resolve().then(() => send({ 'Idempotency-Key': entry.key })).then(result => {
            forget(); return result;
        }, error => {
            entry.promise = null;
            if (error.retryable === false) forget();
            throw error;
        });
        return entry.promise;
    }
}
