#!/usr/bin/env python3
"""Cross-build ScrcpyScript .exe for Windows distribution.

Usage:
    python tools/build_win.py          # Build, output to win/
    python tools/build_win.py --clean  # Clean and rebuild
    python tools/build_win.py --run    # Build and run with Wine

Requires: wine (for running), Python on Wine or cross mingw for real build.
For actual Windows .exe: run this script ON Windows, or use CI (GitHub Actions).
"""

import argparse
import shutil
import subprocess
import sys
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parent.parent
WIN_DIR = PROJECT_ROOT / "win"
SPEC = PROJECT_ROOT / "tools" / "scrcpy_script.spec"
MAIN = PROJECT_ROOT / "scrcpy_script" / "main.py"


def clean() -> None:
    if WIN_DIR.exists():
        shutil.rmtree(WIN_DIR)
    for d in ["build", "dist"]:
        p = PROJECT_ROOT / d
        if p.exists():
            shutil.rmtree(p)
    for f in PROJECT_ROOT.glob("*.spec"):
        f.unlink()
    print("[clean] Done")


def build() -> None:
    clean()

    cmd = [
        sys.executable, "-m", "PyInstaller",
        "--onedir",
        "--name", "scrcpy_script",
        "--distpath", str(WIN_DIR),
        "--workpath", str(PROJECT_ROOT / "build"),
        "--specpath", str(PROJECT_ROOT / "tools"),
        "--clean",
        "--noconfirm",
        str(MAIN),
    ]

    print(f"[build] Running: {' '.join(cmd)}")
    subprocess.run(cmd, check=True, cwd=PROJECT_ROOT)

    # Rename output directory to win/scrcpy_script for clean distribution
    output = WIN_DIR / "scrcpy_script"
    if not output.exists():
        print("[build] Build failed — output not found")
        sys.exit(1)

    # Rename inner exe directory for cleaner structure
    inner = output / "scrcpy_script"
    if inner.is_dir():
        # Move contents one level up
        for item in inner.iterdir():
            shutil.move(str(item), str(output / item.name))
        inner.rmdir()

    print(f"[build] Done → {output}")


def copy_assets() -> None:
    """Copy config and placeholder files alongside the exe."""
    output = WIN_DIR / "scrcpy_script"
    if not output.exists():
        print("[assets] Build first")
        sys.exit(1)

    config_src = PROJECT_ROOT / "scrcpy_script" / "scrcpy_config.conf"
    shutil.copy(config_src, output / "scrcpy_config.conf")

    # Create empty directories for user scripts/templates
    (output / "scripts").mkdir(exist_ok=True)
    (output / "templates").mkdir(exist_ok=True)

    # Copy example scripts
    scripts_src = PROJECT_ROOT / "scrcpy_script" / "scripts"
    if scripts_src.is_dir():
        for d in scripts_src.iterdir():
            if d.is_dir() and not d.name.startswith("_"):
                dest = output / "scripts" / d.name
                if not dest.exists():
                    shutil.copytree(d, dest)

    # Create README
    (output / "README.txt").write_text("""ScrcpyScript — Multi-device Android Automation
=============================================

Quick Start:
  1. Install ADB and add to PATH
  2. Install scrcpy (optional, for native windows)
  3. Place scrcpy-server-v4.0.jar in this directory
  4. Run: scrcpy_script.exe
  5. Or: scrcpy_script.exe --config my.conf --device SERIAL

Scripts:     scripts/     (manifest.py + script.py per subdirectory)
Templates:   templates/   (.png images for template matching)
Config:      scrcpy_config.conf

See README.md for full documentation.
""")

    print(f"[assets] Config + scripts + templates copied to {output}")


def main() -> None:
    parser = argparse.ArgumentParser(description="Build ScrcpyScript for Windows")
    parser.add_argument("--clean", action="store_true", help="Clean only")
    args = parser.parse_args()

    if args.clean:
        clean()
        return

    build()
    copy_assets()


if __name__ == "__main__":
    main()
