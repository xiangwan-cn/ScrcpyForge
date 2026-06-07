"""Single device card with preview, script controls, and log."""
import importlib.util
import subprocess
from pathlib import Path
from types import ModuleType
from typing import Optional

import dearpygui.dearpygui as dpg
import numpy as np

from scrcpy_script.api import ScriptAPI
from scrcpy_script.script.runner import ScriptRunner
from scrcpy_script.ui.log_panel import LogPanel

try:
    from watchdog.observers import Observer
    from watchdog.events import FileSystemEventHandler
    _has_watchdog = True
except ImportError:
    _has_watchdog = False


class DeviceCard:
    def __init__(self, session, scripts_dir: str = "scripts") -> None:
        self._session = session
        self._scripts_dir = scripts_dir
        self._runner: Optional[ScriptRunner] = None
        self._script_modules: list[str] = []
        self._current_script: str = ""
        self._texture_tag = ""
        self._tex_created = False
        self._tex_size = (0, 0)
        self._tex_counter = 2  # start at 2 (0=placeholder, 1=first real frame)
        self._log_panel: Optional[LogPanel] = None
        self._api: Optional[ScriptAPI] = None
        self._observer = None
        self._refresh_scripts()
        self._start_watcher()

    def build(self, parent: str) -> None:
        serial = self._session.serial()
        conn_type = "TCP" if ":" in serial else "USB"
        tag = f"card_{serial}"

        with dpg.texture_registry(tag=f"{tag}_tex_reg"):
            pass

        placeholder_tag = f"tex_{serial}_0"
        dummy = np.zeros(3, dtype=np.float32)
        dpg.add_raw_texture(
            width=1, height=1, default_value=dummy,
            tag=placeholder_tag, format=dpg.mvFormat_Float_rgb,
            parent=f"{tag}_tex_reg",
        )
        self._texture_tag = placeholder_tag

        with dpg.child_window(parent=parent, tag=tag, width=400, height=600,
                              border=True):
            dpg.add_text(tag=f"{tag}_label",
                         default_value=f"{serial} — {self._session.device_name()} [{conn_type}]")
            dpg.add_text(tag=f"{tag}_status", default_value=f"Disconnected [{conn_type}]")

            dpg.add_image(placeholder_tag, tag=f"{tag}_preview",
                          width=360, height=270)

            dpg.add_combo(
                items=self._script_modules, tag=f"{tag}_script",
                default_value="test_tap" if "test_tap" in self._script_modules else (
                    self._script_modules[0] if self._script_modules else ""
                ),
                label="Script", width=150,
            )
            with dpg.group(horizontal=True):
                dpg.add_button(label="Run", callback=lambda: self._on_run(),
                               tag=f"{tag}_run")
                dpg.add_button(label="Stop", callback=lambda: self._on_stop(),
                               tag=f"{tag}_stop", show=False)
                dpg.add_button(label="Screenshot", callback=lambda: self._on_screenshot(),
                               tag=f"{tag}_shot")
                dpg.add_button(label="Open scrcpy", callback=lambda: self._on_open_scrcpy(),
                               tag=f"{tag}_scrcpy")

            self._log_panel = LogPanel(tag=f"{tag}_logpanel")

    def _refresh_scripts(self) -> None:
        self._script_modules = []
        self._script_dirs: dict[str, str] = {}
        scripts_path = Path(self._scripts_dir)
        if scripts_path.is_dir():
            for d in sorted(scripts_path.glob("*")):
                if not d.is_dir() or d.name.startswith("_"):
                    continue
                manifest = d / "manifest.py"
                if not manifest.is_file():
                    continue
                display = d.name
                try:
                    spec = importlib.util.spec_from_file_location(
                        f"_{d.name}_manifest", str(manifest),
                    )
                    if spec and spec.loader:
                        mod = importlib.util.module_from_spec(spec)
                        spec.loader.exec_module(mod)
                        if hasattr(mod, "NAME"):
                            display = mod.NAME
                except Exception:
                    pass
                self._script_modules.append(display)
                self._script_dirs[display] = d.name

    def _load_script(self, name: str) -> Optional[ModuleType]:
        scripts_path = Path(self._scripts_dir)
        dir_name = self._script_dirs.get(name, name)
        manifest = scripts_path / dir_name / "manifest.py"
        if manifest.exists():
            spec = importlib.util.spec_from_file_location(
                f"{dir_name}.manifest", str(manifest),
            )
            if spec is None or spec.loader is None:
                return None
            mod = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(mod)
            return mod
        return None

    def _on_run(self) -> None:
        if self._runner and self._runner.is_running:
            return
        serial = self._session.serial()
        script_name = dpg.get_value(f"card_{serial}_script")
        mod = self._load_script(script_name)
        if mod is None:
            self._session.log(f"[ERROR] Script not found: {script_name}")
            return
        self._runner = ScriptRunner(None)
        self._api = ScriptAPI(self._session, self._runner.stop_event)
        self._runner.set_api(self._api)
        self._runner.run(mod)
        self._session.log(f"[INFO] Running: {script_name}")

    def _on_stop(self) -> None:
        if self._runner:
            self._runner.stop()
            self._runner = None
            self._api = None
            self._session.log("[INFO] Script stopped")

    def _on_open_scrcpy(self) -> None:
        subprocess.Popen(
            ["scrcpy", "-s", self._session.serial()],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )

    def _on_screenshot(self) -> None:
        import scrcpy_script.ui.region_picker as rp
        picker = rp.new_picker(self._session, templates_dir="templates")
        picker.freeze()

    def refresh(self, card_width: int = 400, card_height: int = 600) -> None:
        session = self._session
        serial = session.serial()
        tag = f"card_{serial}"

        # Resize card
        dpg.configure_item(tag, width=card_width, height=card_height)
        preview_w = card_width - 40
        preview_h = card_height - 220  # space for title, buttons, log

        status = f"Connected  {session.fps:.0f} FPS" if session.connected else "Disconnected"
        conn_type = "TCP" if ":" in serial else "USB"
        if dpg.does_item_exist(f"{tag}_status"):
            dpg.set_value(f"{tag}_status", f"{status} [{conn_type}]")

        # Sync run/stop button state with actual script state
        running = self._runner is not None and self._runner.is_running
        if dpg.does_item_exist(f"{tag}_run"):
            dpg.configure_item(f"{tag}_run", show=not running)
        if dpg.does_item_exist(f"{tag}_stop"):
            dpg.configure_item(f"{tag}_stop", show=running)

        frame = session.get_frame()
        if frame is not None:
            self._render_frame(frame, serial, preview_w, preview_h)

        if not session.connected and self._runner and self._runner.is_running:
            self._on_stop()

        if self._log_panel:
            self._log_panel.refresh(session)

    def _render_frame(self, frame: np.ndarray, serial: str,
                      max_w: int = 440, max_h: int = 320) -> None:
        tag = f"card_{serial}"
        reg_tag = f"{tag}_tex_reg"
        img_tag = f"{tag}_preview"
        h, w = frame.shape[:2]

        scale = min(max_w / w, max_h / h)
        dw = int(w * scale)
        dh = int(h * scale)

        data = np.ascontiguousarray(frame[:, :, ::-1]).ravel()
        data = (data / 255.0).astype(np.float32)

        if not self._tex_created:
            # First frame (or after rotation): create texture with correct dimensions
            old_tag = self._texture_tag
            new_tag = f"tex_{serial}_{self._tex_counter}"
            self._tex_counter += 1
            dpg.add_raw_texture(
                width=w, height=h, default_value=data,
                tag=new_tag, format=dpg.mvFormat_Float_rgb,
                parent=reg_tag,
            )
            dpg.configure_item(img_tag, texture_tag=new_tag, width=dw, height=dh)
            if dpg.does_item_exist(old_tag):
                dpg.delete_item(old_tag)
            self._texture_tag = new_tag
            self._tex_created = True
            self._tex_size = (w, h)
        elif (w, h) != self._tex_size:
            # Dimensions changed (rotation): must recreate texture
            self._tex_created = False
            self._tex_size = (w, h)
            old_tag = self._texture_tag
            new_tag = f"tex_{serial}_{self._tex_counter}"
            self._tex_counter += 1
            dpg.add_raw_texture(
                width=w, height=h, default_value=data,
                tag=new_tag, format=dpg.mvFormat_Float_rgb,
                parent=reg_tag,
            )
            dpg.configure_item(img_tag, texture_tag=new_tag, width=dw, height=dh)
            if dpg.does_item_exist(old_tag):
                dpg.delete_item(old_tag)
            self._texture_tag = new_tag
            self._tex_created = True
        else:
            # Same dimensions: just update data
            dpg.set_value(self._texture_tag, data)

    def _start_watcher(self) -> None:
        if not _has_watchdog:
            return
        scripts_path = Path(self._scripts_dir)
        if not scripts_path.is_dir():
            return

        card = self

        class Handler(FileSystemEventHandler):
            def on_modified(self, event):
                if event.src_path.endswith(".py"):
                    card._session.log(f"[INFO] Script changed: {Path(event.src_path).name}")
                    card._refresh_scripts()

        self._observer = Observer()
        self._observer.schedule(Handler(), str(scripts_path), recursive=False)
        self._observer.start()

    def shutdown(self) -> None:
        if self._runner:
            self._runner.stop()
        if self._observer:
            self._observer.stop()
            self._observer.join(timeout=2)
            self._observer = None

    def is_script_running(self) -> bool:
        return self._runner is not None and self._runner.is_running

    def start_script(self) -> None:
        self._on_run()

    def stop_script(self) -> None:
        self._on_stop()
