#!/usr/bin/env python3
"""Firmware simulation (SITL) for the ground station: fetch, build and cache the boards' native firmware, and run it.

The station's "Firmware sim" mode runs the real firmware of every board (FC, RF, PB, GB, AB, VB, DAQ), compiled for
the host by FirmwareSimulator's native SITL engine (docs/native-sitl.md there), instead of the station's own
simplified flight_sim. See docs/firmware-sim.md.

  python3 tools/firmware_sim.py build [--ref dev] [--board flight=<ref> ...] [--fs-ref dev | --fs-dir DIR]
  python3 tools/firmware_sim.py status            # the cache manifest (what firmware would be simulated)
  python3 tools/firmware_sim.py run --out DIR     # the bay alone, for debugging (the supervisor normally starts it)
  python3 tools/firmware_sim.py clean [--all]     # drop build intermediates (or the whole cache)

Cache: $GS_FIRMWARE_SIM_CACHE, default ~/.cache/seds-firmware-sim (about 145 MB once built: seven native board
executables, their symbol tables, the firmware-sim coordinator and the FirmwareSimulator scripts/shim sources).
Board sources, ARM configure trees and cargo targets live under <cache>/work only while building and are deleted
afterwards (a full build peaks at about 1.3 GB there).

Sources come from local clones when they exist (--workspace, default: the directory holding this GroundStation26
checkout) and are otherwise cloned (bare, blobless) into <cache>/mirrors. Boards build SEDSnet from its main branch
unless --sedsnet-ref says otherwise.
"""
from __future__ import annotations

import argparse
import datetime as _dt
import io
import json
import os
import shutil
import subprocess
import sys
import tarfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
GS_ROOT = HERE.parent
NODE_REPO = {
    "flight": "FlightComputer26",
    "rf": "RFBoard26",
    "power": "PowerBoard26",
    "gateway": "gateway-board26",
    "actuator": "ActuatorBoard26",
    "valve": "ValveBoard26",
    "daq": "DAQ-Board",
}
NODES = list(NODE_REPO)
REMOTES = {
    "FirmwareSimulator": "git@github.com:University-at-Buffalo-SEDS/FirmwareSimulator.git",
    "FlightComputer26": "git@github.com:University-at-Buffalo-SEDS/FlightComputer26.git",
    "ValveBoard26": "https://github.com/University-at-Buffalo-SEDS/ValveBoard26.git",
    "ActuatorBoard26": "git@github.com:University-at-Buffalo-SEDS/ActuatorBoard26.git",
    "DAQ-Board": "https://github.com/University-at-Buffalo-SEDS/DAQ-Board.git",
    "gateway-board26": "git@github.com:University-at-Buffalo-SEDS/gateway-board26.git",
    "RFBoard26": "https://github.com/University-at-Buffalo-SEDS/RFBoard26.git",
    "PowerBoard26": "https://github.com/University-at-Buffalo-SEDS/PowerBoard26.git",
    "SEDSnet": "git@github.com:Rylan-Meilutis/SEDSnet.git",
    "SEDSLaunchCore": "git@github.com:University-at-Buffalo-SEDS/SEDSLaunchCore.git",
}
MANIFEST = "manifest.json"


def default_workspace() -> Path:
    """The nearest ancestor of this checkout that holds the board repositories (worktrees live deeper)."""
    for parent in GS_ROOT.parents:
        if (parent / "FlightComputer26" / ".git").exists():
            return parent
    return GS_ROOT.parent
MANIFEST_VERSION = 1


def cache_dir(explicit: str | None = None) -> Path:
    value = explicit or os.environ.get("GS_FIRMWARE_SIM_CACHE")
    return Path(value).expanduser() if value else Path.home() / ".cache" / "seds-firmware-sim"


def log(*parts) -> None:
    print("[firmware-sim]", *parts, flush=True)


def git(repo: Path, *args: str, capture: bool = True) -> str:
    result = subprocess.run(["git", "-C", str(repo), *args], capture_output=capture, text=True)
    if result.returncode:
        raise SystemExit(f"git {' '.join(args)} in {repo} failed: {result.stderr.strip()}")
    return (result.stdout or "").strip()


def resolve_repo(name: str, workspace: Path, cache: Path, fetch: bool) -> Path:
    """A git repository holding `name`: the local clone in the workspace, else a blobless mirror in the cache."""
    local = workspace / name
    if (local / ".git").exists():
        if fetch:
            subprocess.run(["git", "-C", str(local), "fetch", "-q", "origin"], check=False)
        return local
    mirror = cache / "mirrors" / f"{name}.git"
    if not mirror.exists():
        mirror.parent.mkdir(parents=True, exist_ok=True)
        log("cloning", REMOTES[name])
        subprocess.run(["git", "clone", "-q", "--bare", "--filter=blob:none", REMOTES[name], str(mirror)], check=True)
        git(mirror, "config", "remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*")
    if fetch:
        subprocess.run(["git", "-C", str(mirror), "fetch", "-q", "origin"], check=False)
    return mirror


def resolve_ref(repo: Path, ref: str) -> str:
    """A branch name means the remote's branch (origin/<ref>); tags and SHAs are used as given."""
    for candidate in (f"origin/{ref}", ref):
        result = subprocess.run(["git", "-C", str(repo), "rev-parse", "--verify", "-q", f"{candidate}^{{commit}}"],
                                capture_output=True, text=True)
        if result.returncode == 0:
            return result.stdout.strip()
    raise SystemExit(f"{repo.name}: unknown ref {ref!r}")


def describe(repo: Path, sha: str) -> dict:
    subject = git(repo, "log", "-1", "--format=%s", sha)
    when = git(repo, "log", "-1", "--format=%cI", sha)
    tag = subprocess.run(["git", "-C", str(repo), "describe", "--tags", "--always", sha], capture_output=True, text=True)
    return {"commit": sha, "short": sha[:7], "subject": subject, "committed": when,
            "describe": tag.stdout.strip() if tag.returncode == 0 else sha[:7]}


def archive(repo: Path, sha: str, dest: Path) -> None:
    if dest.exists():
        shutil.rmtree(dest)
    dest.mkdir(parents=True)
    data = subprocess.check_output(["git", "-C", str(repo), "archive", sha])
    with tarfile.open(fileobj=io.BytesIO(data)) as tar:
        tar.extractall(dest, filter="data")


def read_manifest(cache: Path) -> dict | None:
    try:
        manifest = json.loads((cache / MANIFEST).read_text())
    except (OSError, ValueError):
        return None
    return manifest if manifest.get("version") == MANIFEST_VERSION else None


def write_manifest(cache: Path, manifest: dict) -> None:
    tmp = cache / f".{MANIFEST}.tmp"
    tmp.write_text(json.dumps(manifest, indent=2) + "\n")
    tmp.replace(cache / MANIFEST)


def dir_size(path: Path) -> int:
    total = 0
    for root, _dirs, files in os.walk(path):
        for name in files:
            try:
                total += (Path(root) / name).stat().st_size
            except OSError:
                pass
    return total


def build_firmware_simulator(cache: Path, fs_src: Path, jobs: int) -> Path:
    """The firmware-sim coordinator binary, built from fs_src into <cache>/bin."""
    target = cache / "work" / "fs-target"
    env = {**os.environ, "CARGO_TARGET_DIR": str(target), "CARGO_BUILD_JOBS": str(jobs)}
    log("building the firmware-sim coordinator")
    subprocess.run(["cargo", "build", "--release", "--quiet", "--bin", "firmware-sim"], cwd=fs_src, env=env, check=True)
    out = cache / "bin" / "firmware-sim"
    out.parent.mkdir(parents=True, exist_ok=True)
    tmp = out.with_suffix(".tmp")
    shutil.copyfile(target / "release" / "firmware-sim", tmp)
    tmp.chmod(0o755)
    tmp.replace(out)
    return out


def cmd_build(a) -> int:
    cache = cache_dir(a.cache)
    cache.mkdir(parents=True, exist_ok=True)
    workspace = Path(a.workspace).resolve()
    t0 = time.monotonic()
    refs = {node: a.ref for node in NODES}
    for item in a.board:
        node, _, ref = item.partition("=")
        if node not in NODE_REPO or not ref:
            raise SystemExit(f"--board expects NODE=REF with NODE one of {', '.join(NODES)}")
        refs[node] = ref
    nodes = a.nodes.split(",") if a.nodes else NODES
    previous = read_manifest(cache) or {}
    manifest = {"version": MANIFEST_VERSION, "built": previous.get("built"), "boards": dict(previous.get("boards", {})),
                "firmware_simulator": previous.get("firmware_simulator"), "sedsnet": previous.get("sedsnet")}

    # FirmwareSimulator: the engine (shim, build script, coordinator, launcher).
    fs_cache = cache / "firmware-simulator"
    if a.fs_dir:
        fs_src = Path(a.fs_dir).resolve()
        fs_info = {"source": str(fs_src), "local_checkout": True}
        try:
            fs_info.update(describe(fs_src, git(fs_src, "rev-parse", "HEAD")))
            fs_info["dirty"] = bool(git(fs_src, "status", "--porcelain", "--untracked-files=no"))
        except SystemExit:
            pass
    else:
        fs_repo = resolve_repo("FirmwareSimulator", workspace, cache, not a.no_fetch)
        sha = resolve_ref(fs_repo, a.fs_ref)
        fs_info = {"ref": a.fs_ref, **describe(fs_repo, sha)}
        if (manifest.get("firmware_simulator") or {}).get("commit") != sha or not (fs_cache / "native").exists():
            log("FirmwareSimulator", a.fs_ref, sha[:7])
            archive(fs_repo, sha, fs_cache)
        fs_src = fs_cache
    if not a.no_coordinator:
        build_firmware_simulator(cache, fs_src, a.jobs)
    manifest["firmware_simulator"] = fs_info

    sdk_repo = resolve_repo("SEDSnet", workspace, cache, not a.no_fetch)
    sdk_sha = resolve_ref(sdk_repo, a.sedsnet_ref)
    lc_repo = resolve_repo("SEDSLaunchCore", workspace, cache, not a.no_fetch)
    manifest["sedsnet"] = {"ref": a.sedsnet_ref, **describe(sdk_repo, sdk_sha)}

    work = cache / "work"
    for node in nodes:
        repo_name = NODE_REPO[node]
        repo = resolve_repo(repo_name, workspace, cache, not a.no_fetch)
        sha = resolve_ref(repo, refs[node])
        have = manifest["boards"].get(node) or {}
        exe = cache / "boards" / node / f"{node}.native"
        if (not a.force and have.get("commit") == sha and have.get("sedsnet") == sdk_sha
                and have.get("engine") == fs_info.get("commit") and exe.exists()):
            log(node, "up to date at", sha[:7])
            continue
        src = work / "src" / repo_name  # build_board.py keys board options on the directory name
        log(node, f"{repo_name} {refs[node]} -> {sha[:7]}")
        archive(repo, sha, src)
        started = time.monotonic()
        cmd = [sys.executable, str(fs_src / "native" / "build_board.py"), str(src), "--out", str(work / "boards"),
               "--jobs", str(a.jobs), "--sdk", f"{sdk_repo}@{sdk_sha}"]
        lc_pin = a.launchcore_ref
        if lc_pin:
            cmd += ["--launchcore", f"{lc_repo}@{resolve_ref(lc_repo, lc_pin)}"]
        elif (workspace / "SEDSLaunchCore").exists() is False:
            cmd += ["--launchcore", f"{lc_repo}@e2eea7d94245168dc63d31e160f4cb7bdc7688c3"]
        if a.factory:
            cmd.append("--factory")
        subprocess.run(cmd, check=True)
        built = work / "boards" / node
        dest = cache / "boards" / node
        if dest.exists():
            shutil.rmtree(dest)
        dest.mkdir(parents=True)
        for name in (f"{node}.native", f"{node}.symbols", "factory.bin"):
            if (built / name).exists():
                shutil.copy2(built / name, dest / name)
        shutil.copyfile(src / "sim" / "board.json", dest / "board.json")
        native = json.loads((built / f"{node}.native.json").read_text())
        manifest["boards"][node] = {
            "repo": repo_name, "ref": refs[node], **describe(repo, sha), "sedsnet": sdk_sha,
            "engine": fs_info.get("commit"), "exe": str(dest / f"{node}.native"),
            "factory_bin": str(dest / "factory.bin") if (dest / "factory.bin").exists() else None,
            "family": native.get("family"), "flash_size": (native.get("memory") or {}).get("flash_size", 524288),
            "build_seconds": round(time.monotonic() - started, 1),
        }
        write_manifest(cache, manifest)
        if not a.keep_work:
            shutil.rmtree(built, ignore_errors=True)
            shutil.rmtree(src, ignore_errors=True)
    manifest["built"] = _dt.datetime.now(_dt.timezone.utc).isoformat(timespec="seconds")
    manifest["engine_dir"] = str(fs_src)
    manifest["coordinator"] = str(cache / "bin" / "firmware-sim")
    write_manifest(cache, manifest)
    if not a.keep_work:
        shutil.rmtree(work, ignore_errors=True)
    log(f"done in {time.monotonic() - t0:.0f} s; cache {cache} is {dir_size(cache) / 1e6:.0f} MB")
    return 0


def cmd_status(a) -> int:
    cache = cache_dir(a.cache)
    manifest = read_manifest(cache)
    if manifest is None:
        print(f"No firmware simulation build in {cache}. Run: python3 tools/firmware_sim.py build")
        return 1
    if a.json:
        print(json.dumps(manifest, indent=2))
        return 0
    fs = manifest.get("firmware_simulator") or {}
    print(f"cache {cache} ({dir_size(cache) / 1e6:.0f} MB), built {manifest.get('built')}")
    print(f"engine FirmwareSimulator {fs.get('describe', fs.get('source'))}{' (dirty)' if fs.get('dirty') else ''}")
    sdk = manifest.get("sedsnet") or {}
    print(f"SEDSnet {sdk.get('ref')} {sdk.get('short')}")
    for node in NODES:
        b = manifest["boards"].get(node)
        print(f"  {node:9s} " + (f"{b['repo']} {b['ref']} {b['short']}  {b['subject']}" if b else "(not built)"))
    return 0


def cmd_clean(a) -> int:
    cache = cache_dir(a.cache)
    targets = [cache] if a.all else [cache / "work", cache / "mirrors"]
    for t in targets:
        if t.exists():
            log("removing", t, f"({dir_size(t) / 1e6:.0f} MB)")
            shutil.rmtree(t)
    return 0


def cmd_run(a) -> int:
    """Start the bay from the cache (as the station supervisor does) and wait."""
    cache = cache_dir(a.cache)
    manifest = read_manifest(cache)
    if manifest is None:
        raise SystemExit("not built: python3 tools/firmware_sim.py build")
    launcher = Path(manifest["engine_dir"]) / "scripts" / "gs_bay.py"
    cmd = [sys.executable, str(launcher), "--manifest", str(cache / MANIFEST), "--out", a.out, "--mode", a.mode]
    cmd += a.extra
    # exec: the launcher replaces this process, so it is the supervisor's child and dies with it
    os.execv(sys.executable, cmd)


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--cache", help="cache directory (default $GS_FIRMWARE_SIM_CACHE or ~/.cache/seds-firmware-sim)")
    sub = p.add_subparsers(dest="cmd", required=True)
    b = sub.add_parser("build", help="fetch the boards at their refs and build the native firmware into the cache")
    b.add_argument("--ref", default="dev", help="default ref for every board (branch, tag or SHA; default dev)")
    b.add_argument("--board", action="append", default=[], metavar="NODE=REF", help="per-board ref override")
    b.add_argument("--nodes", help="comma-separated subset to (re)build")
    b.add_argument("--fs-ref", default="dev", help="FirmwareSimulator ref (default dev)")
    b.add_argument("--fs-dir", help="use this FirmwareSimulator checkout as is (development)")
    b.add_argument("--sedsnet-ref", default="main", help="SEDSnet ref the boards build against (default main)")
    b.add_argument("--launchcore-ref", help="SEDSLaunchCore ref (default: the board's pin)")
    b.add_argument("--workspace", default=str(default_workspace()), help="directory with local clones of the repos")
    b.add_argument("--jobs", type=int, default=max(1, (os.cpu_count() or 4) // 2))
    b.add_argument("--factory", action="store_true", help="also build ARM factory images for first boot (slow)")
    b.add_argument("--force", action="store_true")
    b.add_argument("--no-fetch", action="store_true")
    b.add_argument("--no-coordinator", action="store_true", help="skip building the firmware-sim coordinator")
    b.add_argument("--keep-work", action="store_true", help="keep sources and intermediates under <cache>/work")
    s = sub.add_parser("status", help="show the cached firmware revisions")
    s.add_argument("--json", action="store_true")
    c = sub.add_parser("clean", help="remove build intermediates and mirrors (--all: the whole cache)")
    c.add_argument("--all", action="store_true")
    r = sub.add_parser("run", help="run the bay alone (debugging)")
    r.add_argument("--out", required=True)
    r.add_argument("--mode", default="normal", help="station mode the plant follows (test_fire: stand, hitl: no igniter)")
    r.add_argument("extra", nargs=argparse.REMAINDER)
    a = p.parse_args(argv)
    return {"build": cmd_build, "status": cmd_status, "clean": cmd_clean, "run": cmd_run}[a.cmd](a)


if __name__ == "__main__":
    sys.exit(main())
