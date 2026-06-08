"""Single-device session: protocol launch, PyAV decode, frame queue."""
import queue
import threading
import time
from collections import deque
from typing import Callable, Optional

import av
import numpy as np

from scrcpy_script.protocol.control import (
    build_inject_keycode,
    build_inject_text,
    build_inject_touch,
    build_back_or_screen_on,
    BACK_ACTION,
    TouchAction,
)
from scrcpy_script.protocol.server import launch_server, ANNEX_B_PREFIX

QUEUE_DEPTH = 2
SWIPE_STEP_MS = 16
MIN_SWIPE_STEPS = 2


class DeviceSession:
    def __init__(self, serial: str) -> None:
        self._serial = serial
        self._frame_queue: queue.Queue = queue.Queue(maxsize=QUEUE_DEPTH)
        self._cached_frame: Optional[np.ndarray] = None
        self._video_size = (0, 0)
        self._screen_size = (0, 0)
        self._device_name = ""
        self._rotated = False
        self._connected = False
        self._fps = 0.0
        self._last_fps_update = 0.0
        self._frame_count = 0
        self._logs: deque = deque(maxlen=200)
        self._disconnect_cb: Optional[Callable] = None
        self._session = None
        self._decode_thread: Optional[threading.Thread] = None
        self._stop_decode = False

    def connect(
        self,
        video_port: int,
        control_port: int,
        max_size: int = 1280,
        max_fps: int = 60,
        bit_rate: int = 8000000,
        video_codec: str = "h264",
        jar_path: str = "scrcpy-server-v4.0.jar",
    ) -> bool:
        session, error = launch_server(
            self._serial, video_port, control_port,
            jar_path=jar_path, max_size=max_size,
            max_fps=max_fps, bit_rate=bit_rate,
            video_codec=video_codec,
        )
        if session is None:
            err = f"[ERROR] connect failed: {error}"
            print(err, flush=True)
            self.log(err)
            return False
        self._session = session
        self._device_name = session.device_name or self._serial

        print(
            f"[SESSION] launch_server success "
            f"{self._serial}",
            flush=True,
        )

        self._connected = True
        self._stop_decode = False

        print(
            f"[SESSION] starting decode thread "
            f"{self._serial}",
            flush=True,
        )

        self._decode_thread = threading.Thread(target=self._decode_loop, daemon=True)
        self._decode_thread.start()

        print(
            f"[SESSION] decode thread started "
            f"{self._serial}",
            flush=True,
        )

        self.log(f"[INFO] Connected ({session.device_name})")
        return True

    def disconnect(self) -> None:
        self._connected = False
        self._stop_decode = True
        if self._decode_thread and self._decode_thread.is_alive():
            self._decode_thread.join(timeout=3)
        if self._session:
            self._session.close()
            self._session = None
        while True:
            try:
                self._frame_queue.get_nowait()
            except queue.Empty:
                break
        self._cached_frame = None
        if self._disconnect_cb:
            self._disconnect_cb(self._serial)

    def _decode_loop(self) -> None:
        print(
            f"[DECODE] thread enter "
            f"{self._serial}",
            flush=True,
        )

        codec = av.CodecContext.create("h264", "r")

        print(
            f"[DECODE] codec created "
            f"{self._serial}",
            flush=True,
        )

        try:
            while not self._stop_decode and self._session is not None:
                print(
                    f"[DECODE] waiting packet "
                    f"{self._serial}",
                    flush=True,
                )

                pkt_info = self._session.read_packet()

                print(
                    f"[DECODE] packet="
                    f"{pkt_info is not None} "
                    f"{self._serial}",
                    flush=True,
                )

                if pkt_info is None:
                    print(
                        f"[DECODE] read_packet NONE "
                        f"{self._serial}",
                        flush=True,
                    )

                    self.log("[WARN] Video stream ended")
                    break

                if pkt_info.get("is_session"):
                    new_w = pkt_info.get("video_width", self._video_size[0])
                    new_h = pkt_info.get("video_height", self._video_size[1])
                    if (new_w, new_h) != self._video_size:
                        self.log(f"[INFO] Resolution: {self._video_size} -> ({new_w}, {new_h})")
                    self._video_size = (new_w, new_h)
                    self._screen_size = self._video_size
                    continue

                data = pkt_info.get("data")
                if not data:
                    continue

                if pkt_info.get("is_config"):
                    try:
                        packets = codec.parse(ANNEX_B_PREFIX + data)
                        for pkt in packets:
                            codec.decode(pkt)
                    except Exception as e:
                        self.log(f"[WARN] Config parse error: {e}")
                    continue

                try:
                    packets = codec.parse(ANNEX_B_PREFIX + data)
                    for pkt in packets:
                        frames = codec.decode(pkt)
                        for frame in frames:
                            img = frame.to_ndarray(format="bgr24")
                            self._cached_frame = img
                            try:
                                self._frame_queue.put_nowait(img)
                            except queue.Full:
                                try:
                                    self._frame_queue.get_nowait()
                                    self._frame_queue.put_nowait(img)
                                except queue.Empty:
                                    pass
                            self._update_fps()
                except Exception as e:
                    self.log(f"[WARN] Decode drop: {e}")
        except Exception as e:
            import traceback

            print(
                f"[DECODE] EXCEPTION {self._serial}",
                flush=True,
            )

            traceback.print_exc()

            self.log(f"[ERROR] Decode error: {e}")
        finally:
            print(
                f"[DECODE] EXIT {self._serial}",
                flush=True,
            )

            self._connected = False
            if self._disconnect_cb:
                self._disconnect_cb(self._serial)

    def _update_fps(self) -> None:
        now = time.monotonic()
        self._frame_count += 1
        elapsed = now - self._last_fps_update
        if elapsed >= 1.0:
            self._fps = self._frame_count / elapsed
            self._frame_count = 0
            self._last_fps_update = now

    # ── Frame access ───────────────────────────────────

    def get_frame(self) -> Optional[np.ndarray]:
        try:
            return self._frame_queue.get_nowait()
        except queue.Empty:
            return None

    def cached_frame(self) -> Optional[np.ndarray]:
        return self._cached_frame

    # ── Control injection ─────────────────────────────

    def _send_control(self, data: bytes) -> None:
        if self._session:
            self._session.send_control(data)

    def inject_tap(self, x: int, y: int) -> None:
        w, h = self._screen_size
        self._send_control(build_inject_touch(TouchAction.DOWN, x, y, w, h))
        self._send_control(build_inject_touch(TouchAction.UP, x, y, w, h))

    def inject_swipe(self, x1: int, y1: int, x2: int, y2: int, duration_ms: int = 300) -> None:
        w, h = self._screen_size
        steps = max(MIN_SWIPE_STEPS, duration_ms // SWIPE_STEP_MS)
        for i in range(steps):
            t = i / (steps - 1)
            cx = int(x1 + (x2 - x1) * t)
            cy = int(y1 + (y2 - y1) * t)
            if i == 0:
                action = TouchAction.DOWN
            elif i == steps - 1:
                action = TouchAction.UP
            else:
                action = TouchAction.MOVE
            self._send_control(build_inject_touch(action, cx, cy, w, h))
            if i < steps - 1:
                time.sleep(duration_ms / 1000.0 / steps)

    def inject_touch_down(self, x: int, y: int) -> None:
        w, h = self._screen_size
        self._send_control(build_inject_touch(TouchAction.DOWN, x, y, w, h))

    def inject_touch_up(self, x: int, y: int) -> None:
        w, h = self._screen_size
        self._send_control(build_inject_touch(TouchAction.UP, x, y, w, h))

    def inject_key(self, keycode: int, long_press: bool = False) -> None:
        self._send_control(build_inject_keycode(keycode, 1))
        if long_press:
            time.sleep(0.5)
        self._send_control(build_inject_keycode(keycode, 0))

    def inject_back(self) -> None:
        self._send_control(build_back_or_screen_on(BACK_ACTION))

    def inject_text(self, text: str) -> None:
        self._send_control(build_inject_text(text))

    # ── Properties ─────────────────────────────────────

    def log(self, msg: str) -> None:
        self._logs.append(f"[{time.strftime('%H:%M:%S')}] {msg}")

    def logs(self) -> list[str]:
        return list(self._logs)

    def serial(self) -> str:
        return self._serial

    def device_name(self) -> str:
        return self._device_name or self._serial

    def video_size(self) -> tuple[int, int]:
        return self._video_size

    def screen_size(self) -> tuple[int, int]:
        return self._screen_size

    def is_rotated(self) -> bool:
        return self._rotated

    @property
    def fps(self) -> float:
        return self._fps

    @property
    def is_connected(self) -> bool:
        return self._connected

    def set_disconnect_callback(self, cb: Callable) -> None:
        self._disconnect_cb = cb
