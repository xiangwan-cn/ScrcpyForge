"""Script API exposed to user scripts for device interaction."""
import functools
import threading
import time
from typing import Callable

import cv2
import numpy as np


@functools.lru_cache(maxsize=32)
def _load_template(path: str) -> np.ndarray | None:
    return cv2.imread(path)


class ScriptAPI:
    KEY_BACK = 4
    KEY_HOME = 3
    KEY_ENTER = 66
    KEY_POWER = 26
    KEY_VOLUME_UP = 24
    KEY_VOLUME_DOWN = 25
    KEY_MENU = 82

    def __init__(self, session, stop_event: threading.Event | None = None) -> None:
        self._session = session
        self._stop_event = stop_event or threading.Event()

    # ── Frame ──────────────────────────────────────────

    def capture(self) -> np.ndarray | None:
        return self._session.cached_frame()

    def screen_size(self) -> tuple[int, int]:
        return self._session.screen_size()

    def video_size(self) -> tuple[int, int]:
        return self._session.video_size()

    # ── Template matching ──────────────────────────────

    def find(self, tpl_path: str, threshold: float = 0.8,
             roi: tuple[int, int, int, int] | None = None) -> dict | None:
        frame = self.capture()
        if frame is None:
            return None
        tpl = _load_template(tpl_path)
        if tpl is None:
            return None
        ox, oy = 0, 0
        if roi is not None:
            x1, y1, x2, y2 = roi
            frame = frame[y1:y2, x1:x2]
            ox, oy = x1, y1
            if frame.size == 0:
                return None
        result = cv2.matchTemplate(frame, tpl, cv2.TM_CCOEFF_NORMED)
        _, max_val, _, max_loc = cv2.minMaxLoc(result)
        if max_val < threshold:
            return None
        h, w = tpl.shape[:2]
        return {
            "x": max_loc[0] + w // 2 + ox,
            "y": max_loc[1] + h // 2 + oy,
            "w": w,
            "h": h,
            "confidence": float(max_val),
        }

    def find_all(self, tpl_path: str, threshold: float = 0.8,
                 roi: tuple[int, int, int, int] | None = None) -> list[dict]:
        frame = self.capture()
        if frame is None:
            return []
        tpl = _load_template(tpl_path)
        if tpl is None:
            return []
        ox, oy = 0, 0
        if roi is not None:
            x1, y1, x2, y2 = roi
            frame = frame[y1:y2, x1:x2]
            ox, oy = x1, y1
            if frame.size == 0:
                return []
        result = cv2.matchTemplate(frame, tpl, cv2.TM_CCOEFF_NORMED)
        locations = np.where(result >= threshold)
        h, w = tpl.shape[:2]
        matches = []
        for y, x in zip(locations[0], locations[1]):
            matches.append({
                "x": int(x) + w // 2 + ox,
                "y": int(y) + h // 2 + oy,
                "w": w,
                "h": h,
                "confidence": float(result[y, x]),
            })
        return matches

    def wait_for(self, tpl_path: str, timeout_ms: int = 10000,
                 roi: tuple[int, int, int, int] | None = None) -> dict | None:
        deadline = time.monotonic() + timeout_ms / 1000.0
        while time.monotonic() < deadline and not self._stop_event.is_set():
            result = self.find(tpl_path, roi=roi)
            if result:
                return result
            time.sleep(0.05)
        return None

    # ── Touch (device coordinates) ─────────────────────

    def tap(self, x: int, y: int) -> None:
        self._session.inject_tap(x, y)

    def swipe(
        self, x1: int, y1: int, x2: int, y2: int, duration_ms: int = 300
    ) -> None:
        self._session.inject_swipe(x1, y1, x2, y2, duration_ms)

    def long_press(self, x: int, y: int, duration_ms: int) -> None:
        self._session.inject_touch_down(x, y)
        time.sleep(duration_ms / 1000.0)
        self._session.inject_touch_up(x, y)

    def multi_tap(self, points: list[tuple[int, int]]) -> None:
        for x, y in points:
            self._session.inject_touch_down(x, y)
        time.sleep(0.05)
        for x, y in points:
            self._session.inject_touch_up(x, y)

    # ── Input ──────────────────────────────────────────

    def press_key(self, keycode: int, long_press: bool = False) -> None:
        self._session.inject_key(keycode, long_press)

    def press_back(self) -> None:
        self._session.inject_back()

    def input_text(self, text: str) -> None:
        self._session.inject_text(text)

    # ── Flow ───────────────────────────────────────────

    def wait(self, ms: int) -> None:
        """Sleep for ms milliseconds. Raises StopIteration if script was stopped."""
        deadline = time.monotonic() + ms / 1000.0
        while time.monotonic() < deadline:
            if self._stop_event.is_set():
                raise StopIteration("Script stopped")
            time.sleep(min(0.1, deadline - time.monotonic()))

    def repeat_until(self, fn: Callable[[], bool], timeout_ms: int) -> bool:
        deadline = time.monotonic() + timeout_ms / 1000.0
        while time.monotonic() < deadline:
            if fn():
                return True
            time.sleep(0.05)
        return False

    # ── Output ─────────────────────────────────────────

    def log(self, msg: str) -> None:
        self._session.log(f"[INFO] {msg}")

    def warn(self, msg: str) -> None:
        self._session.log(f"[WARN] {msg}")

    def screenshot(self, path: str) -> None:
        frame = self.capture()
        if frame is not None:
            cv2.imwrite(path, frame)

    def device_serial(self) -> str:
        return self._session.serial()

    def device_name(self) -> str:
        return self._session.device_name()

    def is_rotated(self) -> bool:
        return self._session.is_rotated()
