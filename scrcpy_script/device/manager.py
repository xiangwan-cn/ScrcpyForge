"""Multi-device discovery and lifecycle management."""
import subprocess
import threading
import time
from typing import Callable, Optional

from scrcpy_script.device.session import DeviceSession


class DeviceManager:
    def __init__(self, max_devices: int = 10,
                 port_start: int = 27183, port_end: int = 27282) -> None:
        self._sessions: dict[str, DeviceSession] = {}
        self._poll_interval = 2.0
        self._running = False
        self._poll_thread: Optional[threading.Thread] = None
        self._failed_serials: dict[str, float] = {}
        self._reconnect_cooldown = 30.0
        self._port_start = port_start
        self._port_end = port_end
        self._next_port = port_start
        self._connect_params: dict = {}
        self._connecting: set[str] = set()

    def _alloc_ports(self) -> tuple[int, int]:
        video = self._next_port
        control = self._next_port + 1
        self._next_port += 2
        if self._next_port > self._port_end - 1:
            self._next_port = self._port_start
        return video, control

    def set_connect_params(self, **kwargs) -> None:
        self._connect_params.update(kwargs)

    def discover(self) -> list[str]:
        try:
            result = subprocess.run(
                ["adb", "devices"], capture_output=True, text=True, timeout=5
            )
            lines = result.stdout.strip().split("\n")[1:]
            return [
                line.split("\t")[0].strip()
                for line in lines
                if "\tdevice" in line
            ]
        except Exception:
            return []

    def connect_device(self, serial: str, **kwargs) -> Optional[DeviceSession]:
        if serial in self._connecting:
            return None
        if serial in self._sessions and self._sessions[serial].connected:
            return self._sessions[serial]
        self._connecting.add(serial)

        params = dict(self._connect_params)
        params.update(kwargs)
        params.setdefault("max_size", 1280)
        params.setdefault("max_fps", 60)
        params.setdefault("bit_rate", 8000000)
        params.setdefault("video_codec", "h264")
        params.setdefault("jar_path", "scrcpy-server-v4.0.jar")

        if serial in self._sessions:
            self.remove_session(serial)
        if not self._is_device_authorized(serial):
            self._connecting.discard(serial)
            return None
        video_port, control_port = self._alloc_ports()
        session = DeviceSession(serial)
        if not session.connect(
            video_port=video_port, control_port=control_port,
            max_size=params["max_size"], max_fps=params["max_fps"],
            bit_rate=params["bit_rate"], video_codec=params["video_codec"],
            jar_path=params["jar_path"],
        ):
            self._failed_serials[serial] = time.monotonic()
            self._connecting.discard(serial)
            return None
        session.set_disconnect_callback(self._on_disconnect)
        self._sessions[serial] = session
        self._connecting.discard(serial)
        return session

    def _is_device_authorized(self, serial: str) -> bool:
        try:
            result = subprocess.run(
                ["adb", "devices"], capture_output=True, text=True, timeout=5
            )
            for line in result.stdout.strip().split("\n"):
                parts = line.split("\t")
                if len(parts) == 2 and parts[0].strip() == serial:
                    return parts[1].strip() == "device"
        except Exception:
            pass
        return False

    def _on_disconnect(self, serial: str) -> None:
        print(f"[DeviceManager] _on_disconnect({serial})")
        self._sessions.pop(serial, None)

    def remove_session(self, serial: str) -> None:
        print(f"[DeviceManager] remove_session({serial})")
        session = self._sessions.pop(serial, None)
        if session:
            session.disconnect()

    def remove_all(self) -> None:
        for serial in list(self._sessions.keys()):
            self.remove_session(serial)

    def get_session(self, serial: str) -> Optional[DeviceSession]:
        return self._sessions.get(serial)

    def get_sessions(self) -> list[DeviceSession]:
        return list(self._sessions.values())

    def session_count(self) -> int:
        return len(self._sessions)

    def start_polling(self) -> None:
        self._running = True

        def poll_loop() -> None:
            while self._running:
                try:
                    current = set(self.discover())
                    now = time.monotonic()

                    # Auto-connect newly discovered devices
                    for serial in current:
                        if serial in self._connecting:
                            continue
                        if serial not in self._sessions and serial not in self._failed_serials:
                            self.connect_device(serial, **self._connect_params)
                        elif serial in self._failed_serials:
                            if now - self._failed_serials[serial] >= self._reconnect_cooldown:
                                del self._failed_serials[serial]
                                self.connect_device(serial, **self._connect_params)

                    # Disconnect devices that disappeared from adb
                    for serial in list(self._sessions):
                        if serial not in current and serial not in self._connecting:
                            self.remove_session(serial)
                except Exception:
                    pass
                time.sleep(self._poll_interval)

        self._poll_thread = threading.Thread(target=poll_loop, daemon=True)
        self._poll_thread.start()

    def stop_polling(self) -> None:
        self._running = False
        if self._poll_thread:
            self._poll_thread.join(timeout=3)

    def connect_tcp_device(self, ip_addr: str, port: int = 5555) -> bool:
        try:
            subprocess.run(
                ["adb", "connect", f"{ip_addr}:{port}"],
                capture_output=True, timeout=10
            )
            time.sleep(1.0)
            serials = self.discover()
            for serial in serials:
                if ip_addr in serial or ":" in serial:
                    return self.connect_device(serial) is not None
        except Exception:
            pass
        return False
