"""Check the shared release version without changing Cargo.lock."""

import re
import sys
import tomllib
from pathlib import Path


def check_version(root):
    version = (root / "VERSION").read_text().strip()
    if not re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", version):
        raise ValueError("VERSION must be numeric MAJOR.MINOR.PATCH")
    workspace = tomllib.loads((root / "Cargo.toml").read_text())["workspace"]
    if workspace["package"]["version"] != version:
        raise ValueError("VERSION and workspace.package.version differ")
    lock = tomllib.loads((root / "Cargo.lock").read_text())["package"]
    for member in workspace["members"]:
        package = tomllib.loads((root / member / "Cargo.toml").read_text())["package"]
        if package["version"] != {"workspace": True}:
            raise ValueError(f"{member} must inherit the workspace version")
        entries = [p for p in lock if p["name"] == package["name"] and "source" not in p]
        if len(entries) != 1 or entries[0]["version"] != version:
            raise ValueError(f"Cargo.lock version mismatch: {package['name']}")
    return version


if __name__ == "__main__":
    try:
        print(check_version(Path.cwd()))
    except (ValueError, KeyError, OSError) as error:
        sys.exit(str(error))
