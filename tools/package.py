#!/usr/bin/env python3
"""Build and verify a portable archive using only Python's standard library."""
import argparse
import hashlib
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import tarfile
import tempfile
import tomllib
import zipfile

TARGETS = {
    "x86_64-pc-windows-msvc": ("windows-x64", ".zip"),
    "aarch64-apple-darwin": ("macos-arm64", ".tar.gz"),
    "x86_64-apple-darwin": ("macos-x64", ".tar.gz"),
    "x86_64-unknown-linux-gnu": ("linux-x64", ".tar.gz"),
}


def digest(stream):
    return hashlib.file_digest(stream, "sha256").hexdigest()


def copy_executable(source, destination):
    shutil.copy2(source, destination)
    destination.chmod(0o755)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--target", choices=TARGETS, required=True)
    parser.add_argument("--target-dir", type=Path, default=Path(os.getenv("CARGO_TARGET_DIR", "target")))
    parser.add_argument("--output", type=Path, default=Path("dist"))
    parser.add_argument("--licenses", type=Path, required=True)
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    os.chdir(root)
    version = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))["package"]["version"]
    tag = os.getenv("RELEASE_TAG", "")
    if tag and tag != f"v{version}":
        raise SystemExit(f"Release tag {tag!r} does not match Cargo.toml v{version}")
    platform, extension = TARGETS[args.target]
    release = args.target_dir / args.target / "release"
    args.output.mkdir(parents=True, exist_ok=True)
    archive = args.output / f"oci-pull-v{version}-{platform}-portable{extension}"
    with tempfile.TemporaryDirectory(prefix="oci-pull-package-", dir=args.output) as temporary:
        package = Path(temporary) / "oci-pull"
        package.mkdir()
        for name in ("README.md", "LICENSE"):
            shutil.copy2(root / name, package / name)
        shutil.copy2(args.licenses, package / "THIRD-PARTY-LICENSES.txt")
        suffix = ".exe" if "windows" in args.target else ""
        copy_executable(release / f"oci-pull-cli{suffix}", package / f"oci-pull-cli{suffix}")
        if "apple" in args.target:
            app = package / "OCI Pull.app"
            contents = app / "Contents"
            macos = contents / "MacOS"
            resources = contents / "Resources"
            macos.mkdir(parents=True)
            resources.mkdir()
            copy_executable(release / "oci-pull", macos / "oci-pull")
            shutil.copy2(root / "assets/brand/logo.icns", resources / "logo.icns")
            with (contents / "Info.plist").open("wb") as output:
                plistlib.dump({
                    "CFBundleName": "OCI Pull",
                    "CFBundleDisplayName": "OCI Pull",
                    "CFBundleIdentifier": "io.github.sirlydreamer.oci-pull",
                    "CFBundleExecutable": "oci-pull",
                    "CFBundleIconFile": "logo.icns",
                    "CFBundlePackageType": "APPL",
                    "CFBundleShortVersionString": version,
                    "CFBundleVersion": version,
                    "LSMinimumSystemVersion": "15.0",
                    "NSHighResolutionCapable": True,
                    "NSPrincipalClass": "NSApplication",
                }, output)
            subprocess.run(["codesign", "--force", "--sign", "-", str(app)], check=True)
            subprocess.run(["codesign", "--verify", "--deep", "--strict", str(app)], check=True)
        else:
            copy_executable(release / f"oci-pull{suffix}", package / f"oci-pull{suffix}")
        expected = {}
        for path in sorted(package.rglob("*")):
            if path.is_file():
                with path.open("rb") as stream:
                    expected[path.relative_to(package.parent).as_posix()] = digest(stream)
        if extension == ".zip":
            with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as output:
                for name in expected:
                    output.write(package.parent / name, name)
            with zipfile.ZipFile(archive) as output:
                actual = {name: digest(output.open(name)) for name in output.namelist()}
        else:
            with tarfile.open(archive, "w:gz") as output:
                output.add(package, arcname="oci-pull")
            with tarfile.open(archive, "r:gz") as output:
                actual = {member.name: digest(output.extractfile(member))
                          for member in output.getmembers() if member.isfile()}
                for member in output.getmembers():
                    if member.isfile() and member.name.endswith(("/oci-pull", "/oci-pull-cli")):
                        if member.mode & 0o111 != 0o111:
                            raise RuntimeError(f"Executable permission missing: {member.name}")
        if actual != expected:
            raise RuntimeError("Portable archive verification failed")
    with archive.open("rb") as stream:
        checksum = digest(stream)
    Path(str(archive) + ".sha256").write_text(f"{checksum}  {archive.name}\n", encoding="utf-8")
    print(f"Verified {archive} ({len(expected)} files, SHA-256 {checksum})")


if __name__ == "__main__":
    main()
