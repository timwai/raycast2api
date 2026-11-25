"""Fetch only an explicitly locked release asset and verify before extraction."""
import gzip
import hashlib
import json
import os
from pathlib import Path
import urllib.request

lock = json.loads(Path(__file__).with_name('mihomo-lock.json').read_text())
arch = os.environ.get('TARGETARCH')
if arch not in lock['assets']:
    raise SystemExit('unsupported Mihomo architecture: ' + str(arch))
asset = lock['assets'][arch]
url = 'https://github.com/MetaCubeX/mihomo/releases/download/' + lock['version'] + '/' + asset['file']
with urllib.request.urlopen(url, timeout=60) as response:
    data = response.read(100 * 1024 * 1024)
if hashlib.sha256(data).hexdigest() != asset['sha256']:
    raise SystemExit('Mihomo asset SHA-256 mismatch')
out = Path(os.environ.get('MIHOMO_OUTPUT', '/out/mihomo'))
out.parent.mkdir(parents=True, exist_ok=True)
out.write_bytes(gzip.decompress(data))
out.chmod(0o755)
print('Verified Mihomo', lock['version'], arch)
