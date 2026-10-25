"""Flow release keys: PEM compatibility and Windows current-user DPAPI storage."""
import argparse
import base64
import ctypes
import os
from ctypes import wintypes
from pathlib import Path

from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

HEADER = b"FLOW-DPAPI-v1\n"


def default_key_path():
    return Path.home() / ".codex/secrets/flow/update-v2.dpapi"


def protect(data, decrypt=False):
    if os.name != "nt":
        raise OSError("DPAPI keys require the Windows account that created them")

    class Blob(ctypes.Structure):
        _fields_ = [("size", wintypes.DWORD), ("data", ctypes.POINTER(ctypes.c_ubyte))]

    crypt32 = ctypes.WinDLL("crypt32", use_last_error=True)
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel32.LocalFree.argtypes = [ctypes.c_void_p]
    kernel32.LocalFree.restype = ctypes.c_void_p
    source_buffer = (ctypes.c_ubyte * len(data)).from_buffer_copy(data)
    source = Blob(len(data), source_buffer)
    output = Blob()
    if decrypt:
        function = crypt32.CryptUnprotectData
        function.argtypes = [ctypes.POINTER(Blob), ctypes.c_void_p, ctypes.c_void_p,
                             ctypes.c_void_p, ctypes.c_void_p, wintypes.DWORD,
                             ctypes.POINTER(Blob)]
        description = None
    else:
        function = crypt32.CryptProtectData
        function.argtypes = [ctypes.POINTER(Blob), wintypes.LPCWSTR, ctypes.c_void_p,
                             ctypes.c_void_p, ctypes.c_void_p, wintypes.DWORD,
                             ctypes.POINTER(Blob)]
        description = "Flow update signing key v2"
    function.restype = wintypes.BOOL
    if not function(ctypes.byref(source), description, None, None, None, 1, ctypes.byref(output)):
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        return ctypes.string_at(output.data, output.size)
    finally:
        kernel32.LocalFree(output.data)


def public_bytes(key):
    return key.public_key().public_bytes(serialization.Encoding.Raw,
                                         serialization.PublicFormat.Raw)


def load_key(path):
    data = Path(path).read_bytes()
    if data.startswith(HEADER):
        raw = protect(base64.b64decode(data[len(HEADER):], validate=True), decrypt=True)
        return Ed25519PrivateKey.from_private_bytes(raw)
    key = serialization.load_pem_private_key(data, password=None)
    if not isinstance(key, Ed25519PrivateKey):
        raise ValueError("Flow update signing requires an Ed25519 private key")
    return key


def create_key(path):
    path = Path(path)
    if path.exists():
        raise FileExistsError(f"Key already exists; it was not replaced: {path}")
    key = Ed25519PrivateKey.generate()
    raw = key.private_bytes(serialization.Encoding.Raw, serialization.PrivateFormat.Raw,
                            serialization.NoEncryption())
    encrypted = HEADER + base64.b64encode(protect(raw))
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("xb") as file:
        file.write(encrypted)
    return key


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="Create a Windows-account-encrypted Flow release key")
    parser.add_argument("--key", type=Path, default=default_key_path())
    parser.add_argument("--public-key-out", type=Path, required=True)
    args = parser.parse_args()
    key = create_key(args.key)
    args.public_key_out.write_text(public_bytes(key).hex() + "\n", encoding="ascii")
    print(f"Encrypted key stored at {args.key}; public key written to {args.public_key_out}")
