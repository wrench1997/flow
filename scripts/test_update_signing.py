import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

from update_signing import HEADER, create_key, load_key, public_bytes


class SigningTests(unittest.TestCase):
    @unittest.skipUnless(os.name == "nt", "Windows current-user DPAPI")
    def test_encrypted_roundtrip_and_no_overwrite(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "signing.dpapi"
            key = create_key(path)
            data = path.read_bytes()
            self.assertTrue(data.startswith(HEADER))
            self.assertNotIn(b"PRIVATE KEY", data)
            raw = key.private_bytes(serialization.Encoding.Raw,
                                    serialization.PrivateFormat.Raw,
                                    serialization.NoEncryption())
            self.assertNotIn(raw, data)
            loaded = load_key(path)
            self.assertEqual(public_bytes(key), public_bytes(loaded))
            key.public_key().verify(loaded.sign(b"fixture"), b"fixture")
            with self.assertRaises(FileExistsError):
                create_key(path)
            self.assertEqual(data, path.read_bytes())

    def test_existing_pem_interface(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "legacy.pem"
            key = Ed25519PrivateKey.generate()
            path.write_bytes(key.private_bytes(serialization.Encoding.PEM,
                                               serialization.PrivateFormat.PKCS8,
                                               serialization.NoEncryption()))
            self.assertEqual(public_bytes(key), public_bytes(load_key(path)))

    def test_signer_rejects_wrong_key_before_writing_manifest(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            key = Ed25519PrivateKey.generate()
            path = root / "wrong.pem"
            path.write_bytes(key.private_bytes(serialization.Encoding.PEM,
                                               serialization.PrivateFormat.PKCS8,
                                               serialization.NoEncryption()))
            exe = root / "Flow.exe"
            exe.write_bytes(b"fixture")
            output = root / "update.json"
            result = subprocess.run([sys.executable, str(Path(__file__).with_name("sign-update.py")),
                                     "--exe", str(exe), "--version", "0.4.19",
                                     "--key", str(path), "--out", str(output)],
                                    capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("does not match", result.stderr)
            self.assertFalse(output.exists())


if __name__ == "__main__":
    unittest.main()
