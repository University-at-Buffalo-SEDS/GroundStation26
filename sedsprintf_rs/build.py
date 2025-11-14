#!/usr/bin/env python3
import subprocess
import sys
from pathlib import Path
import shutil
import re


# The line pattern in .gitignore you want to temporarily comment out.
# This is treated as a literal and turned into a whole-line regex.
PYI_IGNORE_LINE = "python-files/sedsprintf_rs_2026/sedsprintf_rs_2026.pyi"
PYI_IGNORE_REGEX = re.compile(rf"^{re.escape(PYI_IGNORE_LINE)}$")


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


def _comment_out_pyi_ignore(gitignore: Path) -> None:
    """
    In-place edit of .gitignore using pure Python + regex:
    comment out any line whose stripped content matches PYI_IGNORE_REGEX,
    as long as it's not already commented out.
    """
    if not gitignore.exists():
        return

    text = gitignore.read_text(encoding="utf-8").splitlines(keepends=True)
    new_lines = []
    changed = False
    matched_lines = []

    for line in text:
        stripped = line.strip()

        # Already a comment? leave it alone
        if stripped.startswith("#"):
            new_lines.append(line)
            continue

        # If the stripped line matches our regex, comment it out
        if PYI_IGNORE_REGEX.fullmatch(stripped):
            # Preserve line ending; comment out the significant part
            commented = "# " + line.lstrip()
            new_lines.append(commented)
            matched_lines.append(stripped)
            changed = True
        else:
            new_lines.append(line)

    if changed:
        print(f"info: Commented out in .gitignore: {matched_lines}")
        gitignore.write_text("".join(new_lines), encoding="utf-8")


def run_with_pyi_unignored(cmd: list[str]) -> None:
    """
    Temporarily comment out the PYI_IGNORE_LINE in .gitignore using pure Python,
    run the given command, and then restore .gitignore.
    """
    gitignore = Path(".gitignore")
    backup = None

    try:
        if gitignore.exists():
            backup = gitignore.with_name(".gitignore.maturin-backup")
            shutil.copy2(gitignore, backup)

            print(f"info: Temporarily commenting out pattern '{PYI_IGNORE_LINE}' in .gitignore")
            _comment_out_pyi_ignore(gitignore)

        print("Running:", " ".join(cmd))
        subprocess.run(cmd, check=True)
    finally:
        if backup and backup.exists():
            print("info: Restoring original .gitignore")
            shutil.move(backup, gitignore)


def install_wheel_file(build_mode: list[str]) -> None:
    # Build wheel with maturin under the .gitignore hack
    cmd_build = ["maturin", "build", *build_mode]
    run_with_pyi_unignored(cmd_build)

    # Install the built wheel
    wheels_dir = Path("target") / "wheels"
    wheels = sorted(wheels_dir.glob("sedsprintf_rs_2026-*.whl"))
    if not wheels:
        raise SystemExit(f"No wheels found in {wheels_dir}")
    wheel = wheels[-1]

    cmd_install = ["uv", "pip", "install", str(wheel)]
    print("Running:", " ".join(cmd_install))
    subprocess.run(cmd_install, check=True)


def main(argv: list[str]) -> None:
    build_mode: list[str] = []
    tests = False
    build_embedded = False
    build_python = False
    build_wheel = False
    develop_wheel = False
    release_build = False
    install_wheel = False
    target = ""
    cmd = []

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
        elif arg == "maturin-install":
            print("Installing Python wheel.")
            install_wheel = True
        elif arg.startswith("target="):
            target = arg.split("=", 1)[1]
            print(f"Target set to: {target}")
        else:
            print(f"Unknown option: {arg}")

    # Decide build args
    ensure_rust_target_installed(
        target if target else "thumbv7em-none-eabihf" if build_embedded else ""
    )
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
                "--target",
                target,
            ]

    # Dispatch to the correct command
    if tests:
        cmd = ["cargo", "test", *build_mode, *build_args]
        print("Running:", " ".join(cmd))
        subprocess.run(cmd, check=True)
    elif build_wheel:
        # maturin build with temporary .gitignore modification
        run_with_pyi_unignored(["maturin", "build", *build_mode])
    elif develop_wheel:
        # maturin develop with temporary .gitignore modification
        run_with_pyi_unignored(["maturin", "develop", *build_mode])
    elif install_wheel:
        install_wheel_file(build_mode)
    else:
        cmd = ["cargo", "build", *build_mode, *build_args]
        print("Running:", " ".join(cmd))
        subprocess.run(cmd, check=True)


if __name__ == "__main__":
    main(sys.argv[1:])
