"""DearPyGui main application window."""
import json
from pathlib import Path
from typing import Optional

import dearpygui.dearpygui as dpg

from scrcpy_script.device.manager import DeviceManager
from scrcpy_script.ui.device_card import DeviceCard
from scrcpy_script.ui import region_picker

CARD_WIDTH = 370
STATE_FILE = Path("last_scripts.json")


def _load_last_scripts() -> dict[str, str]:
    try:
        return json.loads(STATE_FILE.read_text())
    except (FileNotFoundError, json.JSONDecodeError):
        return {}


def _save_last_scripts(data: dict[str, str]) -> None:
    STATE_FILE.write_text(json.dumps(data, indent=2))

# ── dark theme ────────────────────────────────────────
def _set_dark_theme():
    with dpg.theme() as global_theme:
        with dpg.theme_component(dpg.mvAll):
            dpg.add_theme_color(dpg.mvThemeCol_WindowBg, [14, 17, 22])
            dpg.add_theme_color(dpg.mvThemeCol_ChildBg, [20, 24, 35])
            dpg.add_theme_color(dpg.mvThemeCol_FrameBg, [26, 31, 44])
            dpg.add_theme_color(dpg.mvThemeCol_Button, [30, 36, 50])
            dpg.add_theme_color(dpg.mvThemeCol_ButtonHovered, [40, 48, 66])
            dpg.add_theme_color(dpg.mvThemeCol_ButtonActive, [50, 60, 80])
            dpg.add_theme_color(dpg.mvThemeCol_Border, [36, 42, 56])
            dpg.add_theme_color(dpg.mvThemeCol_Text, [215, 221, 229])
            dpg.add_theme_color(dpg.mvThemeCol_TextDisabled, [110, 118, 135])
            dpg.add_theme_color(dpg.mvThemeCol_ScrollbarBg, [20, 24, 35])
            dpg.add_theme_color(dpg.mvThemeCol_ScrollbarGrab, [40, 48, 66])
            dpg.add_theme_color(dpg.mvThemeCol_Header, [30, 36, 50])
            dpg.add_theme_color(dpg.mvThemeCol_HeaderHovered, [40, 48, 66])
            dpg.add_theme_color(dpg.mvThemeCol_TitleBg, [20, 24, 35])
            dpg.add_theme_color(dpg.mvThemeCol_TitleBgActive, [26, 31, 44])
            dpg.add_theme_style(dpg.mvStyleVar_FrameRounding, 5)
            dpg.add_theme_style(dpg.mvStyleVar_ChildRounding, 8)
            dpg.add_theme_style(dpg.mvStyleVar_WindowRounding, 0)
            dpg.add_theme_style(dpg.mvStyleVar_ItemSpacing, 6, 4)
            dpg.add_theme_style(dpg.mvStyleVar_FramePadding, 6, 4)
    dpg.bind_theme(global_theme)


class UiApp:
    def __init__(self) -> None:
        self._manager: Optional[DeviceManager] = None
        self._cards: dict[str, DeviceCard] = {}
        self._running = False
        self._scripts_dir = "scripts"
        self._last_scripts: dict[str, str] = _load_last_scripts()

    def init(self, device_manager: DeviceManager,
             scripts_dir: str = "scripts") -> None:
        self._manager = device_manager
        self._scripts_dir = scripts_dir
        dpg.create_context()
        _set_dark_theme()
        dpg.create_viewport(title="ScrcpyForge", width=1400, height=900)
        dpg.setup_dearpygui()

        with dpg.window(tag="main_window", label="ScrcpyForge",
                        no_title_bar=False):
            with dpg.group(horizontal=True):
                dpg.add_button(label="Scan", callback=self._on_scan)
                dpg.add_button(label="Start All", callback=self._on_start_all)
                dpg.add_button(label="Stop All", callback=self._on_stop_all)
                dpg.add_input_text(tag="tcp_addr_input", width=180,
                                   default_value="192.168.1.100:5555", hint="IP:Port")
                dpg.add_button(label="Connect TCP", callback=self._on_connect_tcp)

            with dpg.group(tag="device_grid", horizontal=True):
                pass

        dpg.set_primary_window("main_window", True)

    def run(self) -> None:
        dpg.show_viewport()
        self._running = True
        while self._running and dpg.is_dearpygui_running():
            self._refresh()
            picker = region_picker.get_active_picker()
            if picker:
                picker.refresh()
            dpg.render_dearpygui_frame()
        dpg.destroy_context()

    def shutdown(self) -> None:
        self._running = False
        for card in self._cards.values():
            card.shutdown()

    def _refresh(self) -> None:
        if not self._manager:
            return
        sessions = self._manager.get_sessions()

        for serial in list(self._cards.keys()):
            if serial not in {s.serial() for s in sessions}:
                card = self._cards.pop(serial, None)
                if card:
                    card.shutdown()
                    tag = f"card_{serial}"
                    if dpg.does_item_exist(tag):
                        dpg.delete_item(tag)
                    tex_reg = f"card_{serial}_tex_reg"
                    if dpg.does_item_exist(tex_reg):
                        dpg.delete_item(tex_reg)

        vp_h = dpg.get_viewport_height()
        card_h = max(400, vp_h - 170)

        for session in sessions:
            serial = session.serial()
            if serial not in self._cards:
                card = DeviceCard(session, scripts_dir=self._scripts_dir,
                                  last_script=self._last_scripts.get(serial, ""))
                card.set_script_run_callback(
                    lambda name, s=serial: (
                        self._last_scripts.__setitem__(s, name),
                        _save_last_scripts(self._last_scripts),
                    )
                )
                card.build("device_grid")
                self._cards[serial] = card

            self._cards[serial].refresh(CARD_WIDTH, card_h)

    def _on_scan(self) -> None:
        if self._manager:
            serials = self._manager.discover()
            for serial in serials:
                if serial not in self._cards:
                    self._manager.connect_device(serial)

    def _on_start_all(self) -> None:
        for card in self._cards.values():
            if not card.is_script_running():
                card.start_script()

    def _on_stop_all(self) -> None:
        for card in list(self._cards.values()):
            card.stop_script()

    def _on_connect_tcp(self) -> None:
        if not self._manager:
            return
        addr = dpg.get_value("tcp_addr_input")
        parts = addr.rsplit(":", 1)
        ip = parts[0]
        port = int(parts[1]) if len(parts) == 2 else 5555
        ok = self._manager.connect_tcp_device(ip, port)
        if not ok:
            with dpg.window(label="Error", modal=True, tag="tcp_err",
                            width=250, height=80, no_resize=True):
                dpg.add_text("Connection failed")
                dpg.add_button(label="OK", callback=lambda: dpg.delete_item("tcp_err"))
