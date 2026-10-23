#!/usr/bin/env python3
"""Collect notices from the exact target's resolved Cargo dependencies."""
import argparse
import json
from pathlib import Path
import re
import subprocess


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--target", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    metadata = json.loads(subprocess.check_output([
        "cargo", "metadata", "--locked", "--format-version", "1",
        "--filter-platform", args.target,
    ]))
    resolved = {node["id"] for node in metadata["resolve"]["nodes"]}
    packages = sorted(
        (p for p in metadata["packages"] if p["id"] in resolved
         and (p["source"] or p["name"] == "oci-client")),
        key=lambda p: (p["name"], p["version"]),
    )
    notices = [f"OCI Pull third-party notices ({args.target})\n",
               "Includes runtime and build dependencies resolved by Cargo.lock.\n"]
    name_pattern = re.compile(r"^(LICENSE|LICENCE|COPYING|NOTICE|OFL|COPYRIGHT)([._-]|$)", re.I)
    for package in packages:
        notices.extend([
            "\n" + "=" * 64 + "\n",
            f"{package['name']} {package['version']}\n",
            f"License: {package['license'] or 'See source'}\n",
            f"Source: {package['repository'] or 'https://crates.io/crates/' + package['name']}\n",
        ])
        root = Path(package["manifest_path"]).parent
        paths = {p for p in root.rglob("*") if p.is_file() and name_pattern.match(p.name)
                 and p.suffix not in (".rs", ".c", ".h") and p.stat().st_size < 200_000}
        if package.get("license_file"):
            paths.add(root / package["license_file"])
        for path in sorted(paths):
            notices.append(f"\n--- {path.relative_to(root).as_posix()} ---\n")
            notices.append(path.read_text(encoding="utf-8", errors="replace") + "\n")
    notices.append("\n--- Lucide icons used in the OCI Pull logo ---\n")
    notices.append(Path("assets/brand/LICENSE-LUCIDE").read_text(encoding="utf-8"))
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text("".join(notices), encoding="utf-8")
    print(f"Collected notices for {len(packages)} dependencies: {args.output}")


if __name__ == "__main__":
    main()
