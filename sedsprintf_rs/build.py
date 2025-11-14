#!/usr/bin/env python3
import subprocess
import sys


def ensure_rust_target_installed(target: str) -> None:
    """Ensure the given Rust target is installed via rustup."""
    if not target:
        # Empty target means "host default" – nothing to install.
        return

    try:
        result = subprocess.run(
            ["rustup", "target", "list", "--installed"],
            check=True,
            capture_output=True,
            text=True,
        )
    except FileNotFoundError:
        print(
            "warning: `rustup` not found; cannot ensure Rust target is installed. "
            "Build may fail if the target is missing.",
            file=sys.stderr,
        )
        return

    installed = {line.strip() for line in result.stdout.splitlines() if line.strip()}
    if target in installed:
        print(f"info: Rust target `{target}` already installed.")
        return

    print(f"info: Rust target `{target}` not installed; running `rustup target add {target}`")
    subprocess.run(["rustup", "target", "add", target], check=True)
    print(f"info: Successfully installed Rust target `{target}`.")


def main(argv: list[str]) -> None:
    build_mode: list[str] = []
    tests = False
    build_embedded = False
    build_python = False
    build_wheel = False
    develop_wheel = False
    release_build = False
    target = ""

    # Parse args in any order
    for arg in argv:
        if arg == "release":
            print("Building release version.")
            release_build = True
        elif arg == "test":
            print("Running tests.")
            tests = True
        elif arg == "embedded":
            print("Building for Embedded target.")
            build_embedded = True
        elif arg == "python":
            print("Building Python bindings.")
            build_python = True
        elif arg == "maturin-build":
            print("Building Python wheel.")
            build_wheel = True
        elif arg == "maturin-develop":
            print("Building and installing Python wheel in development mode.")
            develop_wheel = True
        elif arg.startswith("target="):
            target = arg.split("=", 1)[1]
            print(f"Target set to: {target}")
        else:
            print(f"Unknown option: {arg}")

    # Decide build args
    ensure_rust_target_installed(target if target else "thumbv7em-none-eabihf" if build_embedded else "")
    build_args: list[str] = []
    if release_build:
        build_mode = ["--release"]

    if build_embedded:
        if not target:
            print("info: no target specified using thumbv7em-none-eabihf")
            target = "thumbv7em-none-eabihf"
        build_args = [
            "--no-default-features",
            "--target",
            target,
            "--features",
            "embedded",
        ]
        if release_build:
            build_mode = ["--profile", "release-embedded"]
    elif build_python:
        build_args = [
            "--features",
            "python",
        ]
    else:
        if target:
            build_args = [
                "--target", target
            ]
    # Dispatch to the correct command
    if tests:
        cmd = ["cargo", "test", *build_mode, *build_args]
    elif build_wheel:
        cmd = ["maturin", "build", *build_mode]
    elif develop_wheel:
        cmd = ["maturin", "develop", *build_mode]
    else:
        cmd = ["cargo", "build", *build_mode, *build_args]

    print("Running:", " ".join(cmd))
    subprocess.run(cmd, check=True)


if __name__ == "__main__":
    main(sys.argv[1:])
