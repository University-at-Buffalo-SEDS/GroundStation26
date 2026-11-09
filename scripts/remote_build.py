"""Build the station on a faster remote host and copy the results back.

`build.py --remote HOST` sends the checkout (and the frontend checkout) to
HOST, compiles inside an Ubuntu 24.04 container there, and copies the finished
backend binary and web bundle back to where a local build would put them. The
release profile and every compiler flag are unchanged, so the binary is the
same as a local build; only the machine doing the work differs.

The container matches the Raspberry Pi station image (Ubuntu 24.04, glibc
2.39) and cross-compiles for aarch64 with Ubuntu's own arm64 libraries, so the
binary runs on the Pi whatever distribution the remote host uses. The remote
host needs SSH, rsync and Docker usable by the SSH user.

HOST is `user:password@host`, `user@host` or `host`. A password is handed to
ssh through SSH_ASKPASS from the environment, never on a command line. Prefer
an SSH key: the password stays in this process's environment while it runs.
"""
from __future__ import annotations

import hashlib
import os
import platform
import re
import shlex
import shutil
import socket
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Optional

ENV = "GS_REMOTE_BUILD"
PASSWORD_ENV = "GS_REMOTE_BUILD_PASSWORD"
# Relative to the remote home directory, where ssh and rsync start; rsync no
# longer expands $HOME in remote paths. Docker needs it absolute.
REMOTE_ROOT = ".cache/gs-remote-build"
REMOTE_ROOT_ABS = "$HOME/" + REMOTE_ROOT
TARGET = "aarch64-unknown-linux-gnu"
IMAGE = "gs-station-builder"

DOCKERFILE = r"""
FROM ubuntu:24.04
ARG RUST_VERSION
ARG DX_VERSION
ARG WASM_BINDGEN_VERSION
ARG BINARYEN_VERSION
ARG ESBUILD_VERSION
ENV DEBIAN_FRONTEND=noninteractive \
    RUSTUP_HOME=/opt/rust/rustup \
    CARGO_HOME=/opt/rust/cargo \
    PATH=/opt/rust/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
RUN sed -i 's/^Types: deb$/Types: deb\nArchitectures: amd64/' /etc/apt/sources.list.d/ubuntu.sources \
 && printf 'Types: deb\nURIs: http://ports.ubuntu.com/ubuntu-ports\nSuites: noble noble-updates noble-security\nComponents: main universe\nArchitectures: arm64\nSigned-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg\n' \
      > /etc/apt/sources.list.d/arm64.sources \
 && dpkg --add-architecture arm64 \
 && apt-get update \
 && apt-get install -y --no-install-recommends \
      build-essential gcc-aarch64-linux-gnu g++-aarch64-linux-gnu libc6-dev-arm64-cross \
      clang lld cmake nasm perl pkg-config python3 python3-venv curl ca-certificates git \
      libssl-dev libudev-dev:arm64 \
 && rm -rf /var/lib/apt/lists/*
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path --profile minimal \
      --default-toolchain "$RUST_VERSION" --target aarch64-unknown-linux-gnu --target wasm32-unknown-unknown
RUN curl -sSL "https://github.com/WebAssembly/binaryen/releases/download/version_${BINARYEN_VERSION}/binaryen-version_${BINARYEN_VERSION}-x86_64-linux.tar.gz" \
      | tar xz -C /opt \
 && ln -s "/opt/binaryen-version_${BINARYEN_VERSION}/bin/wasm-opt" /usr/local/bin/wasm-opt
RUN curl -sSL "https://registry.npmjs.org/@esbuild/linux-x64/-/linux-x64-${ESBUILD_VERSION}.tgz" \
      | tar xz -C /tmp \
 && install -m 0755 /tmp/package/bin/esbuild /usr/local/bin/esbuild \
 && rm -rf /tmp/package
RUN cargo install --locked dioxus-cli --version "$DX_VERSION" \
 && cargo install --locked wasm-bindgen-cli --version "$WASM_BINDGEN_VERSION" \
 && rm -rf /opt/rust/cargo/registry /opt/rust/cargo/git \
 && chmod -R a+rwX /opt/rust
"""

# Ubuntu's arm64 toolchain and libraries for the cross build. The linker and
# its flags come from the checkout's .cargo/config.toml, as on the Pi.
CROSS_ENV = {
    "CC_aarch64_unknown_linux_gnu": "aarch64-linux-gnu-gcc",
    "CXX_aarch64_unknown_linux_gnu": "aarch64-linux-gnu-g++",
    "AR_aarch64_unknown_linux_gnu": "aarch64-linux-gnu-ar",
    "PKG_CONFIG_ALLOW_CROSS": "1",
    "PKG_CONFIG_LIBDIR_aarch64_unknown_linux_gnu": "/usr/lib/aarch64-linux-gnu/pkgconfig:/usr/share/pkgconfig",
}

# Settings that change what is built, passed through as given.
PASSTHROUGH_ENV = ("MAPLIBRE_GL_VERSION", "MAPLIBRE_GL_BASE_URL", "CARGO_NET_OFFLINE")


class Remote:
    def __init__(self, spec: str):
        spec = spec.strip()
        if not spec:
            raise SystemExit("--remote needs a host: user:password@host, user@host or host")
        user: Optional[str] = None
        password: Optional[str] = None
        host = spec
        if "@" in spec:
            credentials, host = spec.rsplit("@", 1)
            user, sep, secret = credentials.partition(":")
            password = secret if sep else None
            user = user or None
        if not host or any(c.isspace() for c in host):
            raise SystemExit(f"Invalid --remote host: {host!r}")
        self.destination = f"{user}@{host}" if user else host
        self.password = password
        self._askpass: Optional[Path] = None
        # Unix sockets cap paths near 104 bytes; system temp dirs can exceed it.
        self._control = Path(tempfile.mkdtemp(prefix="gsrb", dir="/tmp")) / "%C"

    def env(self) -> dict[str, str]:
        environment = os.environ.copy()
        if self.password is not None:
            if self._askpass is None:
                fd, name = tempfile.mkstemp(prefix="gs-askpass-", suffix=".sh")
                with os.fdopen(fd, "w") as script:
                    script.write(f'#!/bin/sh\nprintf "%s\\n" "${PASSWORD_ENV}"\n')
                os.chmod(name, 0o700)
                self._askpass = Path(name)
            environment[PASSWORD_ENV] = self.password
            environment["SSH_ASKPASS"] = str(self._askpass)
            environment["SSH_ASKPASS_REQUIRE"] = "force"
            environment.setdefault("DISPLAY", ":0")
        return environment

    def ssh_options(self) -> list[str]:
        # One authenticated connection is reused for every step.
        return ["-o", "ControlMaster=auto", "-o", f"ControlPath={self._control}",
                "-o", "ControlPersist=120", "-o", "StrictHostKeyChecking=accept-new",
                "-o", "ServerAliveInterval=30"]

    def ssh(self, command: str, capture: bool = False) -> str:
        cmd = ["ssh", *self.ssh_options(), self.destination, command]
        if capture:
            return subprocess.run(cmd, env=self.env(), check=True, text=True,
                                  stdout=subprocess.PIPE).stdout
        _stream(cmd, self.env())
        return ""

    def rsync(self, source: str, destination: str, filters: list[str], delete: bool, lock: Optional[str] = None,
              extra: Optional[list[str]] = None) -> None:
        shell = " ".join(shlex.quote(part) for part in ["ssh", *self.ssh_options()])
        cmd = ["rsync", "-az", "--human-readable", "--info=stats1", "-e", shell]
        if lock:
            # Concurrent backend and frontend steps share the frontend copy.
            cmd += ["--rsync-path", f"mkdir -p {_remote_dir(lock)} && flock {lock} rsync"]
        if delete:
            cmd.append("--delete")
        cmd += [f"--filter={rule}" for rule in filters]
        cmd += extra or []
        cmd += [source, destination]
        _stream(cmd, self.env())

    def push(self, local: Path, remote: str) -> None:
        self.ssh(f"mkdir -p {remote}")
        # Exactly what git treats as the checkout (tracked files, including
        # force-added ones under ignore rules, plus untracked unignored files)
        # and the ignored lockfiles, so the remote resolves the same
        # dependencies. Remote build output (target/, dist/) is never touched.
        files = _checkout_files(local)
        with tempfile.NamedTemporaryFile("wb", prefix="gs-remote-files-", delete=False) as listing:
            listing.write(b"\0".join(path.encode() for path in files))
        try:
            self.rsync(f"{local}/", f"{self.destination}:{remote}/",
                       [], delete=False, lock=f"{remote}.sync.lock",
                       extra=["--from0", f"--files-from={listing.name}"])
        finally:
            os.unlink(listing.name)

    def pull(self, remote: str, local: Path, delete: bool = False) -> None:
        self.rsync(f"{self.destination}:{remote}", str(local), [], delete=delete)

    def close(self) -> None:
        subprocess.run(["ssh", *self.ssh_options(), "-O", "exit", self.destination],
                       env=self.env(), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        if self._askpass is not None:
            self._askpass.unlink(missing_ok=True)


def _checkout_files(local: Path) -> list[str]:
    listed = subprocess.run(["git", "-C", str(local), "ls-files", "-z", "--cached", "--others", "--exclude-standard"],
                            check=True, stdout=subprocess.PIPE).stdout
    files = [name.decode() for name in listed.split(b"\0") if name]
    present = [name for name in files if (local / name).is_file()]
    for manifest in [name for name in present if name == "Cargo.toml" or name.endswith("/Cargo.toml")]:
        lock = str(Path(manifest).with_name("Cargo.lock"))
        if (local / lock).is_file() and lock not in present:
            present.append(lock)
    return present


def _remote_dir(path: str) -> str:
    return path.rsplit("/", 1)[0]


def _stream(cmd: list[str], env: dict[str, str]) -> None:
    printable = " ".join(shlex.quote(str(part)) for part in cmd)
    print(f"Running: {printable}", flush=True)
    subprocess.run(cmd, env=env, check=True)


def _version(command: list[str], pattern: str, default: str) -> str:
    try:
        output = subprocess.run(command, capture_output=True, text=True).stdout
    except FileNotFoundError:
        return default
    match = re.search(pattern, output)
    return match.group(1) if match else default


def _dx_esbuild_version() -> str:
    tools = Path.home() / ".local/share/.dx/tools"
    found = sorted(tools.glob("esbuild-*")) if tools.is_dir() else []
    if found:
        return found[-1].name.removeprefix("esbuild-")
    return _version(["esbuild", "--version"], r"(\d+\.\d+\.\d+)", "0.27.3")


def _image(remote: Remote) -> str:
    """The builder image tag, building it on the remote the first time."""
    rust = _version(["rustc", "--version"], r"rustc (\S+)", "stable")
    args = {
        "RUST_VERSION": rust,
        "DX_VERSION": _version(["dx", "--version"], r"dioxus (\S+)", "0.7.10"),
        "WASM_BINDGEN_VERSION": _version(["wasm-bindgen", "--version"], r"wasm-bindgen (\S+)", "0.2.130"),
        "BINARYEN_VERSION": _version(["wasm-opt", "--version"], r"version[ _](\d+)", "126"),
        # dx bundles with esbuild; in a container the frontend build forbids
        # dx downloads, so the image carries the version dx fetches locally.
        "ESBUILD_VERSION": _dx_esbuild_version(),
    }
    key = hashlib.sha256((DOCKERFILE + repr(sorted(args.items()))).encode()).hexdigest()[:16]
    tag = f"{IMAGE}:{key}"
    present = remote.ssh(f"docker image inspect {tag} >/dev/null 2>&1 && echo yes || true", capture=True)
    if present.strip() != "yes":
        print(f"Building the remote builder image {tag} (first use only; several minutes)...", flush=True)
        build_args = " ".join(f"--build-arg {k}={shlex.quote(v)}" for k, v in args.items())
        dockerfile = shlex.quote(DOCKERFILE)
        remote.ssh(f"mkdir -p {REMOTE_ROOT} && flock {REMOTE_ROOT}/image.lock sh -c "
                   + shlex.quote(f"docker image inspect {tag} >/dev/null 2>&1 || "
                                 f"printf %s {dockerfile} | docker build {build_args} -t {tag} -"))
    return tag


def _workspace(local: Path) -> str:
    """Per-client remote copy of one local checkout."""
    name = re.sub(r"[^A-Za-z0-9._-]", "_", f"{socket.gethostname()}-{local.resolve().name}")
    return f"ws/{name}"


def _container(remote: Remote, tag: str, workdir: str, command: str, env: dict[str, str]) -> None:
    variables = {
        "HOME": "/build/home",
        "CARGO_HOME": "/build/cargo-home",
        "PATH": "/build/cargo-home/bin:/opt/rust/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        **{name: os.environ[name] for name in PASSTHROUGH_ENV if name in os.environ},
        **env,
    }
    flags = " ".join(f"-e {shlex.quote(f'{k}={v}')}" for k, v in variables.items())
    remote.ssh(f"mkdir -p {REMOTE_ROOT}/home {REMOTE_ROOT}/cargo-home && "
               f"docker run --rm --init --user \"$(id -u):$(id -g)\" {flags} "
               f"-v \"{REMOTE_ROOT_ABS}\":/build -w {shlex.quote('/build/' + workdir)} {tag} "
               f"bash -c {shlex.quote(command)}")


def _check_local_target() -> None:
    machine = platform.machine().lower()
    if platform.system() != "Linux" or machine not in {"aarch64", "arm64"}:
        raise SystemExit(f"--remote builds the aarch64 Linux station binary; this host is {platform.system()} {machine}. "
                         "Run it on the Raspberry Pi station.")


def requested() -> Optional[str]:
    return os.environ.get(ENV) or None


def build_backend(repo_root: Path, features: list[str], debug: bool) -> None:
    """Cross-compile groundstation_backend remotely and install it locally."""
    _check_local_target()
    remote = Remote(requested() or "")
    try:
        tag = _image(remote)
        workspace = _workspace(repo_root)
        remote.push(repo_root, f"{REMOTE_ROOT}/{workspace}")
        env = dict(CROSS_ENV)
        frontend = os.environ.get("GS_FRONTEND_SOURCE_DIR")
        if frontend:
            frontend_workspace = _workspace(Path(frontend))
            remote.push(Path(frontend), f"{REMOTE_ROOT}/{frontend_workspace}")
            env["GS_FRONTEND_SOURCE_DIR"] = f"/build/{frontend_workspace}"
        cargo = ["cargo", "build", "-p", "groundstation_backend", "--target", TARGET]
        if not debug:
            cargo.insert(2, "--release")
        for feature in features:
            cargo += ["--features", feature]
        _container(remote, tag, f"{workspace}/backend", " ".join(shlex.quote(p) for p in cargo), env)
        profile = "debug" if debug else "release"
        target_dir = Path(os.environ.get("CARGO_TARGET_DIR") or repo_root / "target")
        local = target_dir / profile / "groundstation_backend"
        local.parent.mkdir(parents=True, exist_ok=True)
        # Copy beside the running binary, then replace it in one rename.
        staged = local.with_name(".groundstation_backend.remote")
        remote.pull(f"{REMOTE_ROOT}/{workspace}/target/{TARGET}/{profile}/groundstation_backend", staged)
        staged.chmod(0o755)
        staged.replace(local)
        print(f"Remote backend build installed: {local}", flush=True)
    finally:
        remote.close()


def build_frontend(checkout: Path, script: Path, args: list[str]) -> None:
    """Run the frontend web build remotely and bring its dist/ back."""
    remote = Remote(requested() or "")
    try:
        tag = _image(remote)
        workspace = _workspace(checkout)
        remote.push(checkout, f"{REMOTE_ROOT}/{workspace}")
        relative = script.resolve().parent.relative_to(checkout.resolve())
        workdir = f"{workspace}/{relative}" if str(relative) != "." else workspace
        remote_args = [arg for arg in args if not arg.startswith("log=")]
        command = " ".join(shlex.quote(p) for p in ["python3", script.name, *remote_args])
        _container(remote, tag, workdir, command, {})
        dist = script.resolve().parent / "dist"
        dist.mkdir(parents=True, exist_ok=True)
        remote.pull(f"{REMOTE_ROOT}/{workdir}/dist/", dist, delete=True)
        print(f"Remote frontend build copied to {dist}", flush=True)
    finally:
        remote.close()


def take_argument(args: list[str]) -> tuple[list[str], Optional[str]]:
    """Remove `--remote HOST`, `--remote=HOST` or `remote=HOST` from args."""
    rest: list[str] = []
    spec: Optional[str] = None
    items = iter(args)
    for arg in items:
        if arg in {"--remote", "remote"}:
            spec = next(items, None)
            if spec is None:
                raise SystemExit("--remote needs a host: user:password@host, user@host or host")
        elif arg.startswith(("--remote=", "remote=")):
            spec = arg.split("=", 1)[1]
        else:
            rest.append(arg)
    return rest, spec


if __name__ == "__main__":
    sys.exit("Use build.py --remote HOST")
