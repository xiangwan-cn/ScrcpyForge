"""Scrollable log panel for a single device."""
import dearpygui.dearpygui as dpg


class LogPanel:
    def __init__(self, tag: str, label: str = "Log") -> None:
        self._tag = tag
        self._log_tag = f"{tag}_log"
        with dpg.child_window(tag=self._tag, label=label, height=-1):
            dpg.add_text(tag=self._log_tag, default_value="", wrap=0)

    def refresh(self, session: "DeviceSession") -> None:
        if not dpg.does_item_exist(self._log_tag):
            return
        logs = session.logs()
        if logs:
            dpg.set_value(self._log_tag, "\n".join(reversed(logs[-50:])))
