"""Sign Flow's update manifest. Private keys must never be committed or published."""
import argparse, hashlib, json
from pathlib import Path
from cryptography.hazmat.primitives import serialization
p=argparse.ArgumentParser()
p.add_argument('--exe',required=True)
p.add_argument('--version',required=True)
p.add_argument('--key',required=True)
p.add_argument('--out',required=True)
a=p.parse_args()
exe=Path(a.exe)
key=serialization.load_pem_private_key(Path(a.key).read_bytes(),password=None)
expected=bytes.fromhex((Path(__file__).resolve().parent.parent/'assets/update-public-key.hex').read_text().strip())
actual=key.public_key().public_bytes(serialization.Encoding.Raw,serialization.PublicFormat.Raw)
if actual!=expected: raise SystemExit('Signing key does not match embedded public key')
payload=json.dumps({'version':a.version,'url':f'https://github.com/wrench1997/flow/releases/download/v{a.version}/Flow.exe','sha256':hashlib.sha256(exe.read_bytes()).hexdigest(),'size':exe.stat().st_size},separators=(',',':'))
Path(a.out).write_text(json.dumps({'payload':payload,'signature':key.sign(payload.encode()).hex()},indent=2),encoding='utf-8')
print('Signed update manifest created')
