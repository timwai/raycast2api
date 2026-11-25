"""Run inside an isolated container with the pinned core; never uses account credentials."""
import collections
import http.client
import http.server
import json
import os
import pathlib
import subprocess
import tempfile
import threading
import time
import urllib.request

COUNTS = collections.Counter()
SERVERS = []


def endpoint(label):
    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            COUNTS[label] += 1
            body = label.encode()
            self.send_response(200)
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        def do_CONNECT(self):
            self.send_response(200)
            self.end_headers()
            self.rfile.readline()
            while self.rfile.readline().strip():
                pass
            COUNTS[label] += 1
            body = label.encode()
            self.connection.sendall(b'HTTP/1.1 200 OK\r\nContent-Length: ' + str(len(body)).encode() + b'\r\nConnection: close\r\n\r\n' + body)
            self.close_connection = True
        def log_message(self, *_):
            pass
    class Server(http.server.ThreadingHTTPServer):
        def get_request(self):
            result = super().get_request()
            if label == 'unused':
                COUNTS['unexpected_protocol_connection'] += 1
            return result
    server = Server(('127.0.0.1', 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    SERVERS.append(server)
    return server.server_port


def controller(path, method='GET', body=None):
    c = http.client.HTTPConnection('127.0.0.1', 19090, timeout=3)
    c.request(method, path, body=None if body is None else json.dumps(body),
              headers={'Authorization': 'Bearer contract-secret', 'Content-Type': 'application/json'})
    r = c.getresponse()
    data = r.read()
    assert r.status < 300, (method, path, r.status, data)
    c.close()
    return json.loads(data) if data else None


def through(port, target):
    c = http.client.HTTPConnection('127.0.0.1', port, timeout=3)
    try:
        c.request('GET', f'http://127.0.0.1:{target}/contract')
        r = c.getresponse()
        body = r.read().decode()
        return body if r.status == 200 else None
    except (OSError, http.client.HTTPException):
        return None
    finally:
        c.close()


def main():
    core = os.environ.get('MIHOMO_BINARY', '/work/target/mihomo-contract/mihomo-linux-arm64')
    version = subprocess.check_output([core, '-v'], text=True)
    assert 'v1.19.34' in version, version
    node1, node2, direct = endpoint('node1'), endpoint('node2'), endpoint('direct')
    unused = endpoint('unused')
    import socket
    udp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    udp.bind(('127.0.0.1', unused))
    def observe_udp():
        while True:
            try: udp.recvfrom(65535)
            except OSError: return
            COUNTS['unexpected_protocol_udp'] += 1
    threading.Thread(target=observe_udp, daemon=True).start()
    with tempfile.TemporaryDirectory(prefix='mihomo-contract-') as work:
        root = pathlib.Path(work)
        def config(generation, mode='normal'):
            folder = root / generation
            folder.mkdir()
            provider = folder / 'provider.yaml'
            nodes = [{'name': 'rpnode_a', 'type': 'http', 'server': '127.0.0.1', 'port': node1},
                     {'name': 'rpnode_b', 'type': 'http', 'server': '127.0.0.1', 'port': node2}]
            uuid = '00000000-0000-4000-8000-000000000001'
            samples = [
                {'type': 'socks5'},
                {'type': 'ss', 'cipher': 'aes-128-gcm', 'password': 'fixture'},
                {'type': 'vmess', 'uuid': uuid, 'alterId': 0, 'cipher': 'auto', 'network': 'ws', 'ws-opts': {'path': '/fixture'}},
                {'type': 'vless', 'uuid': uuid, 'network': 'ws', 'ws-opts': {'path': '/fixture'}},
                {'type': 'trojan', 'password': 'fixture', 'sni': 'example.invalid'},
                {'type': 'hysteria2', 'password': 'fixture', 'sni': 'example.invalid'},
                {'type': 'tuic', 'uuid': uuid, 'password': 'fixture', 'congestion-controller': 'bbr', 'alpn': ['h3']}
            ]
            nodes += [dict(sample, name='sample_' + sample['type'], server='127.0.0.1', port=unused) for sample in samples]
            provider.write_text(json.dumps({'proxies': nodes if mode != 'empty' else []}))
            groups = []
            for ident in ['a', 'b']:
                group = {'name': 'rp_acc_' + ident, 'type': 'select', 'interval': 0,
                         'empty-fallback': 'REJECT', 'default-selected': 'rpnode_' + ident,
                         'use': ['rp_sub'], 'filter': '^rpnode_' + ident + '$'}
                if mode == 'missing' and ident == 'a':
                    group['filter'] = '^rpnode_missing$'
                if mode == 'disabled' and ident == 'a':
                    group = {'name': 'rp_acc_a', 'type': 'select', 'proxies': ['REJECT'],
                             'default-selected': 'REJECT', 'empty-fallback': 'REJECT', 'interval': 0}
                groups.append(group)
            value = {'port': 0, 'socks-port': 0, 'mixed-port': 0, 'allow-lan': False,
                     'mode': 'rule', 'log-level': 'debug', 'external-controller': '127.0.0.1:19090',
                     'secret': 'contract-secret', 'geo-auto-update': False,
                     'dns': {'enable': False}, 'tun': {'enable': False}, 'sniffer': {'enable': False},
                     'ntp': {'enable': False}, 'profile': {'store-selected': False, 'store-fake-ip': False},
                     'proxy-providers': {'rp_sub': {'type': 'file', 'path': str(provider),
                                                  'health-check': {'enable': False}}},
                     'proxy-groups': groups,
                     'listeners': [{'name': 'rp_listener_' + x, 'type': 'mixed', 'listen': '127.0.0.1',
                                    'port': 18000+i, 'proxy': 'rp_acc_'+x} for i, x in enumerate(['a','b'])],
                     'rules': ['MATCH,REJECT']}
            path = folder / 'config.yaml'
            path.write_text(json.dumps(value))
            result = subprocess.run([core, '-t', '-d', work, '-f', str(path)], capture_output=True, text=True)
            assert result.returncode == 0, result.stdout + result.stderr
            return path
        initial = config('g1')
        log = (root / 'core.log').open('w+')
        process = subprocess.Popen([core, '-d', work, '-f', str(initial)], stdout=log, stderr=log)
        try:
            for _ in range(100):
                try:
                    controller('/version')
                    for ident in ['a', 'b']:
                        ready = controller('/proxies/rp_acc_' + ident)
                        assert ready['all'] == ['rpnode_' + ident]
                    break
                except Exception:
                    if process.poll() is not None:
                        log.seek(0)
                        raise AssertionError(log.read())
                    time.sleep(.05)
            else:
                raise AssertionError('controller did not become ready')
            for ident in ['a','b']:
                group = controller('/proxies/rp_acc_' + ident)
                assert group['all'] == ['rpnode_' + ident], group
                assert group['now'] == 'rpnode_' + ident, group
            assert through(18000, direct) == 'node1', dict(COUNTS)
            assert through(18001, direct) == 'node2'
            for index, mode in enumerate(['missing', 'disabled', 'empty'], 2):
                path = config('g' + str(index), mode)
                controller('/configs?force=true', 'PUT', {'path': str(path)})
                group = controller('/proxies/rp_acc_a')
                assert group['all'] == ['REJECT'], group
                before = COUNTS.copy()
                for _ in range(7):
                    assert through(18000, direct) is None, (mode, COUNTS)
                assert COUNTS == before, (mode, 'unexpected outbound', COUNTS, before)
            restored = config('g5')
            controller('/configs?force=true', 'PUT', {'path': str(restored)})
            assert through(18000, direct) == 'node1', dict(COUNTS)
            before = COUNTS.copy()
            time.sleep(float(os.environ.get('MIHOMO_IDLE_SECONDS', '3')))
            assert COUNTS == before, ('automatic probe', COUNTS, before)
            assert COUNTS['direct'] == 0, COUNTS
            assert COUNTS['unexpected_protocol_connection'] == 0, COUNTS
            assert COUNTS['unexpected_protocol_udp'] == 0, COUNTS
            assert COUNTS['node1'] == 2 and COUNTS['node2'] == 1, COUNTS
            print(json.dumps({'version': 'v1.19.34', 'result': 'passed', 'outbound_counts': dict(COUNTS)}))
        except Exception:
            log.flush()
            log.seek(0)
            print(log.read())
            raise
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
            log.close()
    udp.close()
    for server in SERVERS:
        server.shutdown()


if __name__ == '__main__':
    main()
