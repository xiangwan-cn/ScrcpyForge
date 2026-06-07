"""ScrcpyScript — multi-device Android automation with Python."""
import argparse
import sys
from pathlib import Path

# Allow running as both `python scrcpy_script/main.py` and `python -m scrcpy_script.main`
_script_dir = Path(__file__).resolve().parent
_project_root = _script_dir.parent
if str(_project_root) not in sys.path:
    sys.path.insert(0, str(_project_root))

from scrcpy_script.config import Config
from scrcpy_script.device.manager import DeviceManager
from scrcpy_script.ui.app import UiApp


def _resolve_root() -> Path:
    """Find project root (containing scrcpy_config.conf or scrcpy-server jar)."""
    cwd = Path.cwd()
    for base in [cwd, cwd.parent, cwd.parent.parent]:
        if (base / "scrcpy_config.conf").exists() or (base / "scrcpy_script" / "scrcpy_config.conf").exists():
            return base
    return cwd


def main() -> None:
    parser = argparse.ArgumentParser(description="ScrcpyScript")
    parser.add_argument("--config", default="scrcpy_config.conf",
                        help="Path to config file")
    parser.add_argument("--device", help="Serial of device to connect on startup")
    parser.add_argument("--script", help="Script to load on startup")
    parser.add_argument("--jar", help="Path to scrcpy-server-v4.0.jar")
    parser.add_argument("--adb", help="Path to adb binary (unused, uses system adb)")
    args = parser.parse_args()

    root = _resolve_root()

    config = Config()
    config_path = Path(args.config)
    if config_path.exists():
        config.load(str(config_path))
    else:
        for base in [root, root / "scrcpy_script"]:
            if (base / args.config).exists():
                config.load(str(base / args.config))
                break

    jar_path = args.jar
    if not jar_path:
        for candidate in [
            root / config.jar_path,
            root / "third_party" / config.jar_path,
            root / "scrcpy_script" / config.jar_path,
        ]:
            if candidate.exists():
                jar_path = str(candidate)
                break
    if not jar_path:
        jar_path = str(root / config.jar_path)
    if Path(config.scripts_dir).is_absolute():
        scripts_dir = config.scripts_dir
    else:
        scripts_dir = str(root / config.scripts_dir)

    manager = DeviceManager(
        max_devices=config.max_devices,
        port_start=config.port_start,
        port_end=config.port_end,
    )
    manager.set_connect_params(
        jar_path=jar_path,
        max_size=config.max_size,
        max_fps=config.max_fps,
        bit_rate=config.bit_rate,
        video_codec=config.video_codec,
    )
    manager.start_polling()

    app = UiApp()
    app.init(manager, scripts_dir=scripts_dir)

    # --device: force-connect immediately (poll loop also auto-connects within 2s)
    if args.device:
        manager.connect_device(args.device)

    try:
        app.run()
    finally:
        manager.stop_polling()
        manager.remove_all()
        app.shutdown()


if __name__ == "__main__":
    main()
