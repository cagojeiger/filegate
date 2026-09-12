"""Assemble the public CLI asset inventory after all native builds succeed."""

import hashlib
import json
import re
import sys
from pathlib import Path

TARGETS = (
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
)


def build_manifest(directory, version):
    if not re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", version):
        raise ValueError("expected numeric MAJOR.MINOR.PATCH")
    assets = {}
    for target in TARGETS:
        name = f"gscli-{target}"
        path = directory / name
        data = path.read_bytes()
        digest = hashlib.sha256(data).hexdigest()
        if not data or (directory / f"{name}.sha256").read_text().strip() != digest:
            raise ValueError(f"empty asset or checksum mismatch: {name}")
        assets[target] = {"name": name, "sha256": digest, "size": len(data)}
    return {
        "schema_version": 1,
        "version": version,
        "repository": "cagojeiger/filegate",
        "assets": assets,
    }


if __name__ == "__main__":
    print(json.dumps(build_manifest(Path(sys.argv[1]), sys.argv[2]), indent=2))
