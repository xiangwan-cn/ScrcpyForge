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
    build_set_screen_power_mode,
    BACK_ACTION,
    SCREEN_POWER_OFF,
    SCREEN_POWER_ON,
    TouchAction,
)
from scrcpy_script.protocol.server import launch_server

QUEUE_DEPTH = 2
SWIPE_STEP_MS = 16
MIN_SWIPE_STEPS = 2
FALLBACK_TIMEOUT = 5.0
ANNEX_B_PREFIX = b"\x00\x00\x00\x01"
ANNEX_B_3B = b"\x00\x00\x01"


def _yuv420p_to_bgr(frame) -> np.ndarray:
    """Convert a YUV420P VideoFrame to BGR24 numpy array via OpenCV."""
    import cv2

    h, w = frame.height, frame.width
    h2, w2 = h // 2, w // 2
    total_h = h * 3 // 2

    i420 = np.zeros((total_h, w), dtype=np.uint8)

    y = np.frombuffer(bytes(frame.planes[0]), dtype=np.uint8)
    y = y.reshape((h, frame.planes[0].line_size))[:, :w]
    i420[:h, :] = y

    u = np.frombuffer(bytes(frame.planes[1]), dtype=np.uint8)
    u = u.reshape((h2, frame.planes[1].line_size))[:, :w2]
    u = u.ravel()
    u_rows = h2 * w2 // w
    i420[h : h + u_rows, :] = u[: u_rows * w].reshape((u_rows, w))

    v = np.frombuffer(bytes(frame.planes[2]), dtype=np.uint8)
    v = v.reshape((h2, frame.planes[2].line_size))[:, :w2]
    v = v.ravel()
    v_rows = h2 * w2 // w
    i420[h + u_rows : h + u_rows * 2, :] = v[: v_rows * w].reshape((v_rows, w))

    return cv2.cvtColor(i420, cv2.COLOR_YUV2BGR_I420)


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
        video_encoder: str = "",
        jar_path: str = "scrcpy-server-v4.0.jar",
    ) -> bool:
        session, error = launch_server(
            self._serial, video_port, control_port,
            jar_path=jar_path, max_size=max_size,
            max_fps=max_fps, bit_rate=bit_rate,
            video_codec=video_codec,
            video_encoder=video_encoder,
        )
        if session is None:
            err = f"[ERROR] connect failed: {error}"
            self.log(err)
            return False
        self._session = session
        self._device_name = session.device_name or self._serial
        self._connected = True
        self._stop_decode = False
        self._decode_thread = threading.Thread(target=self._decode_loop, daemon=True)
        self._decode_thread.start()
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
        codec = None
        has_frame = False
        need_reinit = False
        first_pkt_at = time.monotonic()
        pending_config: Optional[bytes] = None
        try:
            while not self._stop_decode and self._session is not None:
                if not has_frame and time.monotonic() - first_pkt_at > FALLBACK_TIMEOUT:
                    self.log("[WARN] No frames — requesting encoder fallback")
                    self._connected = False
                    self._stop_decode = True
                    if self._disconnect_cb:
                        self._disconnect_cb(self._serial)
                    return

                pkt_info = self._session.read_packet()
                if pkt_info is None:
                    self.log("[WARN] Video stream ended")
                    break

                if pkt_info.get("is_session"):
                    new_w = pkt_info.get("video_width", 0)
                    new_h = pkt_info.get("video_height", 0)
                    old_size = self._video_size
                    self._video_size = (new_w, new_h)
                    self._screen_size = self._video_size
                    if old_size == (0, 0) or need_reinit:
                        codec = av.CodecContext.create("h264", "r")
                        codec.flags |= 0x8000  # AV_CODEC_FLAG_LOW_DELAY
                        codec.width = new_w
                        codec.height = new_h
                        codec.pix_fmt = "yuv420p"
                        pending_config = None
                        need_reinit = False
                    if old_size != (0, 0) and (new_w, new_h) != old_size:
                        self.log(f"[INFO] Resolution: {old_size} -> ({new_w}, {new_h})")
                    continue

                if codec is None:
                    continue

                data = pkt_info.get("data")
                if not data:
                    continue

                if pkt_info.get("is_config"):
                    if not data.startswith(ANNEX_B_PREFIX) and not data.startswith(ANNEX_B_3B):
                        data = ANNEX_B_PREFIX + data
                    pending_config = data
                    continue

                if pending_config:
                    data = pending_config + data
                    pending_config = None

                try:
                    packet = av.Packet(data)
                    if pkt_info.get("is_keyframe"):
                        packet.is_keyframe = True
                    for frame in codec.decode(packet):
                        img = _yuv420p_to_bgr(frame)
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
                        has_frame = True
                except Exception:
                    pass
        except Exception as e:
            self.log(f"[ERROR] Decode error: {e}")
        finally:
            self._connected = False
            self._stop_decode = True
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

    def toggle_screen(self) -> None:
        self._screen_on = not getattr(self, "_screen_on", True)
        mode = SCREEN_POWER_ON if self._screen_on else SCREEN_POWER_OFF
        self._send_control(build_set_screen_power_mode(mode))
        self.log(f"[INFO] Screen {'on' if self._screen_on else 'off'}")

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

    @property
    def is_screen_on(self) -> bool:
        return getattr(self, "_screen_on", True)

    def set_disconnect_callback(self, cb: Callable) -> None:
        self._disconnect_cb = cb
