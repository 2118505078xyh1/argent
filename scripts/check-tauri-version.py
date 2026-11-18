#!/usr/bin/env python3
"""Fail when the Tauri prototype drifts from the workspace version.

    python scripts/check-tauri-version.py
"""

import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def toml_version(path: Path, section: str) -> str:
    text = path.read_text(encoding="utf-8")
    match = re.search(
        rf"\[{re.escape(section)}\]\s*\n(?:.*\n)*?version\s*=\s*\"([^\"]+)\"", text
    )
    if not match:
        raise SystemExit(f"no version under [{section}] in {path}")
    return match.group(1)


def main() -> None:
    versions = {
        "workspace Cargo.toml": toml_version(ROOT / "Cargo.toml", "workspace.package"),
        "tauri Cargo.toml": toml_version(
            ROOT / "crates" / "tauri-app" / "Cargo.toml", "package"
        ),
        "tauri.conf.json": json.loads(
            (ROOT / "crates" / "tauri-app" / "tauri.conf.json").read_text(encoding="utf-8")
        )["version"],
        "ui/package.json": json.loads(
            (ROOT / "crates" / "tauri-app" / "ui" / "package.json").read_text(
                encoding="utf-8"
            )
        )["version"],
    }
    unique = set(versions.values())
    for name, version in versions.items():
        print(f"{name}: {version}")
    if len(unique) != 1:
        print("ERROR: versions disagree", file=sys.stderr)
        raise SystemExit(1)
    print("OK: all versions match")


if __name__ == "__main__":
    main()
