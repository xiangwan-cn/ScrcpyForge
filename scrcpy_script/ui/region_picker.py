"""Coordinate and region picker for template creation."""
from __future__ import annotations

from pathlib import Path

import cv2
import dearpygui.dearpygui as dpg
import numpy as np

_picker_counter = 0
_active_picker = None


def new_picker(session: "DeviceSession", templates_dir: str = "templates") -> "RegionPicker":
    global _active_picker
    picker = RegionPicker(session, templates_dir)
    _active_picker = picker
    return picker


def get_active_picker() -> "RegionPicker | None":
    return _active_picker


class RegionPicker:
    def __init__(self, session, templates_dir: str = "templates") -> None:
        global _picker_counter
        self._session = session
        self._templates_dir = Path(templates_dir)
        self._frozen_frame: np.ndarray | None = None
        self._id = _picker_counter
        _picker_counter += 1
        self._win_tag = f"picker_win_{self._id}"
        self._tex_tag = f"picker_tex_{self._id}"
        self._tex_reg = f"picker_tex_reg_{self._id}"
        self._name_tag = f"picker_name_{self._id}"
        self._coord_tag = f"picker_coord_{self._id}"
        self._drawlist_tag = f"picker_dl_{self._id}"
        self._fw = 0
        self._fh = 0
        self._dw = 0
        self._dh = 0
        self._sel_start = None
        self._last_win_w = 0
        self._last_win_h = 0

    def freeze(self) -> None:
        self._frozen_frame = self._session.cached_frame()
        if self._frozen_frame is not None:
            self._frozen_frame = self._frozen_frame.copy()
        self._build_window()

    def unfreeze(self) -> None:
        global _active_picker
        self._frozen_frame = None
        self._sel_start = None
        if dpg.does_item_exist(self._win_tag):
            dpg.delete_item(self._win_tag)
        if _active_picker is self:
            _active_picker = None

    def refresh(self) -> None:
        if self._frozen_frame is None or not dpg.does_item_exist(self._drawlist_tag):
            return

        win_rect = dpg.get_item_rect_size(self._win_tag)
        win_w, win_h = win_rect[0], win_rect[1]
        if win_w > 0 and win_h > 0 and (win_w != self._last_win_w or win_h != self._last_win_h):
            self._last_win_w = win_w
            self._last_win_h = win_h
            controls_h = 120
            max_w = max(100, win_w - 30)
            max_h = max(100, win_h - controls_h)
            scale = min(max_w / self._fw, max_h / self._fh)
            self._dw = int(self._fw * scale)
            self._dh = int(self._fh * scale)
            dpg.configure_item(self._drawlist_tag, width=self._dw, height=self._dh)
            self._sel_start = None
            self._redraw_image()

        if self._sel_start is None:
            if dpg.is_mouse_button_clicked(0) and dpg.is_item_hovered(self._drawlist_tag):
                mouse_pos = dpg.get_mouse_pos(local=True)
                dl_pos = dpg.get_item_pos(self._drawlist_tag)
                lx = mouse_pos[0] - dl_pos[0]
                ly = mouse_pos[1] - dl_pos[1]
                if 0 <= lx < self._dw and 0 <= ly < self._dh:
                    x, y = self._img_to_device(lx, ly)
                    self._sel_start = (x, y)
        else:
            self._draw_selection()
            if dpg.is_mouse_button_released(0):
                self._end_selection()

    def _redraw_image(self) -> None:
        dpg.delete_item(self._drawlist_tag, children_only=True)
        dpg.draw_image(self._tex_tag, (0, 0), (self._dw, self._dh),
                       uv_min=(0, 0), uv_max=(1, 1), parent=self._drawlist_tag)

    def _draw_selection(self) -> None:
        mouse_pos = dpg.get_mouse_pos(local=True)
        dl_pos = dpg.get_item_pos(self._drawlist_tag)
        lx = mouse_pos[0] - dl_pos[0]
        ly = mouse_pos[1] - dl_pos[1]
        x, y = self._img_to_device(lx, ly)
        sx, sy = self._sel_start
        x1, x2 = min(sx, x), max(sx, x)
        y1, y2 = min(sy, y), max(sy, y)

        rx1 = x1 * self._dw // max(self._fw, 1)
        ry1 = y1 * self._dh // max(self._fh, 1)
        rx2 = x2 * self._dw // max(self._fw, 1)
        ry2 = y2 * self._dh // max(self._fh, 1)

        self._redraw_image()
        dpg.draw_rectangle((rx1, ry1), (rx2, ry2),
                           color=(0, 255, 0, 255), thickness=2,
                           fill=(0, 255, 0, 30), parent=self._drawlist_tag)

    def _end_selection(self) -> None:
        mouse_pos = dpg.get_mouse_pos(local=True)
        dl_pos = dpg.get_item_pos(self._drawlist_tag)
        lx = mouse_pos[0] - dl_pos[0]
        ly = mouse_pos[1] - dl_pos[1]
        x, y = self._img_to_device(lx, ly)
        sx, sy = self._sel_start or (0, 0)
        self._sel_start = None

        x1, x2 = min(sx, x), max(sx, x)
        y1, y2 = min(sy, y), max(sy, y)
        w, h = x2 - x1, y2 - y1
        if w > 5 and h > 5:
            name = dpg.get_value(self._name_tag).strip()
            if name:
                region = self._frozen_frame[y1:y2, x1:x2]
                self._templates_dir.mkdir(parents=True, exist_ok=True)
                path = self._templates_dir / f"{name}.png"
                cv2.imwrite(str(path), region)
                self._session.log(f"[INFO] Saved region {w}x{h}: {path.name}")
            dpg.set_value(self._coord_tag, f"Saved: ({x1},{y1}) {w}x{h}")
        else:
            dpg.set_value(self._coord_tag, f"Clicked: ({x}, {y})")

        self._redraw_image()

    def _build_window(self) -> None:
        if dpg.does_item_exist(self._win_tag):
            dpg.delete_item(self._win_tag)

        vp_w = dpg.get_viewport_width()
        vp_h = dpg.get_viewport_height()
        self._fh, self._fw = self._frozen_frame.shape[:2]

        controls_h = 110
        max_img_w = vp_w - 100
        max_img_h = vp_h - 160
        scale = min(max_img_w / self._fw, max_img_h / self._fh, 1.0)
        self._dw = int(self._fw * scale)
        self._dh = int(self._fh * scale)
        win_w = self._dw + 30
        win_h = self._dh + controls_h

        pos_x = max(0, (vp_w - win_w) // 2)
        pos_y = max(0, (vp_h - win_h) // 2)

        data = np.ascontiguousarray(self._frozen_frame[:, :, ::-1]).ravel()
        data = (data / 255.0).astype(np.float32)

        with dpg.window(
            label=f"Snapshot — {self._session.serial()}",
            width=win_w, height=win_h,
            tag=self._win_tag, pos=[pos_x, pos_y],
            on_close=self.unfreeze,
            no_collapse=True, modal=True,
        ):
            with dpg.texture_registry(tag=self._tex_reg):
                dpg.add_raw_texture(
                    width=self._fw, height=self._fh, default_value=data,
                    tag=self._tex_tag, format=dpg.mvFormat_Float_rgb,
                )
            with dpg.drawlist(width=self._dw, height=self._dh, tag=self._drawlist_tag):
                dpg.draw_image(self._tex_tag, (0, 0), (self._dw, self._dh),
                               uv_min=(0, 0), uv_max=(1, 1))

            dpg.add_text(tag=self._coord_tag, default_value="")
            with dpg.group(horizontal=True):
                dpg.add_input_text(label="", tag=self._name_tag,
                                   default_value="template", width=110)
                dpg.add_button(label="Save Full", callback=self._on_save_full)
                dpg.add_button(label="Close", callback=self.unfreeze)

    def _img_to_device(self, px: float, py: float) -> tuple[int, int]:
        x = int(px * self._fw / max(self._dw, 1))
        y = int(py * self._fh / max(self._dh, 1))
        return min(max(x, 0), self._fw - 1), min(max(y, 0), self._fh - 1)

    def _on_save_full(self) -> None:
        name = dpg.get_value(self._name_tag).strip()
        if not name or self._frozen_frame is None:
            return
        self._templates_dir.mkdir(parents=True, exist_ok=True)
        path = self._templates_dir / f"{name}.png"
        cv2.imwrite(str(path), self._frozen_frame)
        self._session.log(f"[INFO] Saved: {path.name}")
        self.unfreeze()
