#!/usr/bin/env python3
"""Reject a production source that silently differs from the declared core pin."""
import hashlib
import json
from pathlib import Path
import re
import sys
import tomllib


def check(root):
    manifest = tomllib.loads((root / "Cargo.toml").read_text())
    dependency = manifest["dependencies"]["jterm_core"]
    revision = dependency.get("rev", "")
    if not re.fullmatch(r"[0-9a-f]{40}", revision) or "path" in dependency:
        raise ValueError("Production jterm_core must use one exact Git revision, not a path")
    if dependency.get("git") != "https://github.com/beamiter/jterm_core.git":
        raise ValueError("Unexpected production core repository")
    for section in ("dependencies", "dev-dependencies", "build-dependencies"):
        other = manifest.get(section, {}).get("jterm_core")
        if other and (other.get("git") != dependency["git"] or other.get("rev") != revision or "path" in other):
            raise ValueError(f"{section} resolves a different shared core")
    configs = [root / ".cargo/config", root / ".cargo/config.toml"]
    if (root / ".cargo").is_symlink():
        raise ValueError("Cargo config directory must not be a symlink")
    for config in configs:
        if config.is_symlink():
            raise ValueError("Cargo config must not be a symlink")
        if config.is_file():
            data = tomllib.loads(config.read_text())
            for overrides in data.get("patch", {}).values():
                if "jterm_core" in overrides:
                    raise ValueError("A committed Cargo config overrides the production core")
            if data.get("paths"):
                raise ValueError("Committed Cargo paths can override the production source")
            if data.get("replace") or any("replace-with" in source for source in data.get("source", {}).values()):
                raise ValueError("Committed Cargo config replaces a production source")
    for overrides in manifest.get("patch", {}).values():
        if "jterm_core" in overrides:
            raise ValueError("Cargo.toml overrides the production core")
    locked = tomllib.loads((root / "Cargo.lock").read_text())
    cores = [package for package in locked["package"] if package["name"] == "jterm_core"]
    expected = f"git+{dependency['git']}?rev={revision}#{revision}"
    if len(cores) != 1 or cores[0].get("source") != expected:
        raise ValueError("Cargo.lock does not resolve the exact declared production core")
    sources = json.loads((root / "packaging/flatpak/cargo-sources.json").read_text())
    git_sources = [entry for entry in sources if entry.get("type") == "git" and entry.get("url", "").removesuffix(".git") == dependency["git"].removesuffix(".git")]
    if len(git_sources) != 1 or git_sources[0].get("commit") != revision:
        raise ValueError("Flatpak resolves a different core commit")
    flake = (root / "flake.nix").read_text()
    if expected not in flake or not re.search(r'"jterm_core-0\.2\.0"\s*=\s*"sha256-[A-Za-z0-9+/]{43}="', flake):
        raise ValueError("Nix lacks the exact core source and fixed-output hash")
    if "jterm-core-src.tar.gz" in flake:
        raise ValueError("Nix still stages the historical candidate bundle")
    receipt = json.loads((root / "packaging/jterm-core-production.json").read_text())
    if receipt.get("repository") != dependency["git"] or receipt.get("revision") != revision:
        raise ValueError("Production source receipt disagrees with the manifest")
    if f'"jterm_core-0.2.0" = "{receipt.get("source_nar_sha256")}";' not in flake:
        raise ValueError("Nix hash disagrees with the production source receipt")
    origin = json.loads((root / "packaging/jterm-core-origin.json").read_text())
    # The audit export is historical, not a build input. Validate its own
    # receipt rather than silently relabelling it when production is repinned.
    archive_revision = (root / "packaging/jterm-core-revision").read_text().strip()
    if not re.fullmatch(r"[0-9a-f]{40}", archive_revision) or archive_revision == "0" * 40 or origin.get("published_core_commit") != archive_revision:
        raise ValueError("Audit bundle revision disagrees with its historical receipt")
    archive_tree = origin.get("published_core_tree", "")
    archive_nar = origin.get("source_nar_sha256", "")
    if not re.fullmatch(r"[0-9a-f]{40}", archive_tree) or archive_tree == "0" * 40 or not re.fullmatch(r"sha256-[A-Za-z0-9+/]{43}=", archive_nar):
        raise ValueError("Audit bundle source identity is malformed")
    if archive_revision == revision and (archive_tree != receipt.get("tree") or archive_nar != receipt.get("source_nar_sha256")):
        raise ValueError("Audit bundle source identity disagrees with production")
    archive_hash = hashlib.sha256((root / "packaging/jterm-core-src.tar.gz").read_bytes()).hexdigest()
    if origin.get("archive_sha256") != archive_hash or (root / "packaging/jterm-core-src.tar.gz.sha256").read_text().strip() != f"{archive_hash}  jterm-core-src.tar.gz":
        raise ValueError("Audit bundle checksum disagrees with its receipt")
    for name in ("host-adapter-bundle", "process-adapter-bundle"):
        bundle = json.loads((root / f"vendor/{name}.json").read_text())
        if bundle.get("public_core_base") != revision:
            raise ValueError(f"{name} receipt names a different production core")
    fixture = root / "scripts/link-recovery"
    fixture_core = tomllib.loads((fixture / "Cargo.toml").read_text())["dependencies"]["jterm_core"]
    if fixture_core.get("git") != dependency["git"] or fixture_core.get("rev") != revision or "path" in fixture_core:
        raise ValueError("Link-recovery fixture manifest names a different core")
    fixture_cores = [p for p in tomllib.loads((fixture / "Cargo.lock").read_text())["package"] if p["name"] == "jterm_core"]
    if len(fixture_cores) != 1 or fixture_cores[0].get("source") != expected:
        raise ValueError("Link-recovery fixture lock names a different core")
    return revision


if __name__ == "__main__":
    try:
        revision = check(Path(__file__).resolve().parents[1])
    except (ValueError, KeyError, OSError) as error:
        sys.exit(f"Core source check failed: {error}")
    print(f"Production core source agrees across Cargo, lock, Flatpak and Nix: {revision}")
