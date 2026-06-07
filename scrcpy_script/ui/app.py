"""DearPyGui main application window."""
from typing import Optional

import dearpygui.dearpygui as dpg

from scrcpy_script.device.manager import DeviceManager
from scrcpy_script.ui.device_card import DeviceCard
from scrcpy_script.ui import region_picker


class UiApp:
    def __init__(self) -> None:
        self._manager: Optional[DeviceManager] = None
        self._cards: dict[str, DeviceCard] = {}
        self._running = False
        self._scripts_dir = "scripts"

    def init(self, device_manager: DeviceManager,
             scripts_dir: str = "scripts") -> None:
        self._manager = device_manager
        self._scripts_dir = scripts_dir
        dpg.create_context()
        dpg.create_viewport(title="ScrcpyScript", width=1400, height=900)
        dpg.set_global_font_scale(0.85)
        dpg.setup_dearpygui()

        with dpg.window(tag="main_window", label="ScrcpyScript",
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

        vp_w = dpg.get_viewport_width()
        vp_h = dpg.get_viewport_height()

        for session in sessions:
            serial = session.serial()
            if serial not in self._cards:
                card = DeviceCard(session, scripts_dir=self._scripts_dir)
                card.build("device_grid")
                self._cards[serial] = card

            # Card height fills viewport, width wraps preview aspect ratio
            frame = session.cached_frame()
            if frame is not None:
                fh, fw = frame.shape[:2]
                card_h = max(350, vp_h - 140)
                preview_h = card_h - 220
                preview_w = int(preview_h * fw / fh) if fh > 0 else 400
                max_preview_w = min(500, vp_w - 80)
                preview_w = min(preview_w, max_preview_w)
                card_w = preview_w + 40
                card_w = max(card_w, 320)
            else:
                card_w = 400
                card_h = 600

            self._cards[serial].refresh(card_w, card_h)

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
