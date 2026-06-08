"""scrcpy v4.0 server launch via ADB + video packet reader."""
import random
import socket
import subprocess
import time
from typing import Optional


SERVER_PATH = "/data/local/tmp/scrcpy-server-v4.0.jar"
SERVER_CLASS = "com.genymobile.scrcpy.Server"
VERSION = "4.0"
DEVICE_NAME_LENGTH = 64
PACKET_HEADER_SIZE = 12

# Packet flags
PKT_FLAG_SESSION = 0x80
PKT_FLAG_CONFIG = 0x40
PKT_FLAG_KEYFRAME = 0x20

# H.264 Annex B start code
ANNEX_B_PREFIX = b"\x00\x00\x00\x01"


class ScrcpySession:
    def __init__(self) -> None:
        self.serial: str = ""
        self.scid: int = 0
        self.video_port: int = 0
        self.control_port: int = 0
        self._video_sock: Optional[socket.socket] = None
        self._control_sock: Optional[socket.socket] = None
        self.device_name: str = ""
        self.video_width: int = 0
        self.video_height: int = 0

    def close(self) -> None:
        for attr in ("_video_sock", "_control_sock"):
            sock = getattr(self, attr, None)
            if sock:
                try:
                    sock.close()
                except OSError:
                    pass
                setattr(self, attr, None)

    def send_control(self, data: bytes) -> None:
        if self._control_sock:
            try:
                self._control_sock.sendall(data)
            except OSError:
                pass

    def recv_video(self, n: int) -> bytes:
        data = bytearray()
        while len(data) < n:
            try:
                chunk = self._video_sock.recv(n - len(data))
            except (socket.timeout, OSError):
                return bytes(data)
            if not chunk:
                return bytes(data)
            data.extend(chunk)
        return bytes(data)

    def read_packet(self) -> Optional[dict]:
        header = self.recv_video(PACKET_HEADER_SIZE)
        if len(header) < PACKET_HEADER_SIZE:
            return None

        flags = header[0]
        packet = {
            "is_session": bool(flags & PKT_FLAG_SESSION),
            "is_config": bool(flags & PKT_FLAG_CONFIG),
            "is_keyframe": bool(flags & PKT_FLAG_KEYFRAME),
        }

        if packet["is_session"]:
            w = int.from_bytes(header[4:8], "big")
            h = int.from_bytes(header[8:12], "big")
            packet["video_width"] = w
            packet["video_height"] = h
            return packet

        pts = int.from_bytes(header[0:8], "big") & 0x1FFFFFFFFFFFFFFF
        packet["pts"] = pts

        data_size = int.from_bytes(header[8:12], "big")
        if data_size == 0 or data_size > 16 * 1024 * 1024:
            return packet

        data = self.recv_video(data_size)
        packet["data"] = data
        return packet


def _generate_scid() -> int:
    return random.randint(1, 0x7FFFFFFF)


def _generate_scid_hex() -> str:
    return f"{_generate_scid():08x}"


def _adb(serial: str, args: str) -> subprocess.CompletedProcess:
    return subprocess.run(
        f"adb -s {serial} {args}",
        shell=True, capture_output=True, text=True, timeout=30,
    )


def launch_server(
    serial: str,
    video_port: int,
    control_port: int,
    jar_path: str = "scrcpy-server-v4.0.jar",
    max_size: int = 1280,
    bit_rate: int = 8000000,
    max_fps: int = 60,
    video_codec: str = "h264",
    video_encoder: str = "",
    stay_awake: bool = True,
) -> tuple[Optional[ScrcpySession], Optional[str]]:
    return _launch_server(serial, video_port, control_port, jar_path,
                           max_size, bit_rate, max_fps, video_codec,
                           video_encoder, stay_awake)


def _launch_server(
    serial: str,
    video_port: int,
    control_port: int,
    jar_path: str = "scrcpy-server-v4.0.jar",
    max_size: int = 1280,
    bit_rate: int = 8000000,
    max_fps: int = 60,
    video_codec: str = "h264",
    video_encoder: str = "",
    stay_awake: bool = True,
) -> tuple[Optional[ScrcpySession], Optional[str]]:
    # Kill stale servers (best-effort)
    try:
        subprocess.run(
            ["adb", "-s", serial, "shell", "pkill -9 -f app_process.*scrcpy"],
            capture_output=True, timeout=5,
        )
    except Exception:
        pass
    time.sleep(0.5)

    # Push jar
    r = _adb(serial, f'push "{jar_path}" {SERVER_PATH}')
    if r.returncode != 0:
        return None, f"Push failed: {r.stderr.strip()}"

    scid_hex = _generate_scid_hex()

    # Remove stale forwards
    subprocess.run(f"adb -s {serial} forward --remove-all", shell=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    # Forward ports
    for port, label in [(video_port, "video"), (control_port, "control")]:
        r = _adb(serial, f"forward tcp:{port} localabstract:scrcpy_{scid_hex}")
        if r.returncode != 0:
            return None, f"{label} forward failed: {r.stderr.strip()}"

    # Launch server
    cmd = (
        f"adb -s {serial} shell "
        f"CLASSPATH={SERVER_PATH} "
        f"app_process / {SERVER_CLASS} {VERSION} "
        f"scid={scid_hex} "
        f"video=true audio=false control=true "
        f"tunnel_forward=true send_frame_meta=true send_dummy_byte=true "
        f"cleanup=false "
        f"max_size={max_size} "
        f"video_codec={video_codec}"
    )
    if video_encoder:
        cmd += f" video_encoder={video_encoder}"
    if bit_rate > 0:
        cmd += f" video_bit_rate={bit_rate}"
    if max_fps > 0:
        cmd += f" max_fps={max_fps}"
    if stay_awake:
        cmd += " stay_awake=true"
    subprocess.Popen(cmd, shell=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(2.0)

    session = ScrcpySession()
    session.serial = serial
    session.video_port = video_port
    session.control_port = control_port

    # Connect video socket with retry
    try:
        deadline = time.monotonic() + 6.0
        connected = False
        while time.monotonic() < deadline:
            try:
                video_sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
                try:
                    video_sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
                except OSError:
                    pass
                video_sock.connect(("127.0.0.1", video_port))
                connected = True
                break
            except OSError:
                try:
                    video_sock.close()
                except OSError:
                    pass
                time.sleep(0.1)
        if not connected:
            return None, "Video socket connect timeout"
    except Exception:
        return None, "Video socket error"

    # Read dummy byte
    try:
        video_sock.settimeout(3.0)
    except OSError:
        pass
    try:
        dummy = video_sock.recv(1)
        if len(dummy) != 1:
            video_sock.close()
            return None, "Dummy byte read failed"
    except OSError:
        video_sock.close()
        return None, "Dummy byte timeout"
    try:
        video_sock.settimeout(None)
    except OSError:
        pass

    # Connect control socket with retry
    control_sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    try:
        control_sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    except OSError:
        pass
    ctrl_connected = False
    ctrl_deadline = time.monotonic() + 3.0
    while time.monotonic() < ctrl_deadline:
        try:
            control_sock.connect(("127.0.0.1", control_port))
            ctrl_connected = True
            break
        except OSError:
            time.sleep(0.1)
    if not ctrl_connected:
        video_sock.close()
        control_sock.close()
        return None, "Control socket connect timeout"

    # Read device info (64 bytes name) from video socket
    name_bytes = bytearray()
    while len(name_bytes) < DEVICE_NAME_LENGTH:
        try:
            chunk = video_sock.recv(DEVICE_NAME_LENGTH - len(name_bytes))
        except OSError:
            video_sock.close()
            control_sock.close()
            return None, "Device info read failed"
        if not chunk:
            video_sock.close()
            control_sock.close()
            return None, "Device info truncated"
        name_bytes.extend(chunk)
    session.device_name = bytes(name_bytes).decode("utf-8", errors="replace").rstrip("\x00")

    # Read and discard 4-byte codec ID
    codec_buf = bytearray()
    while len(codec_buf) < 4:
        try:
            chunk = video_sock.recv(4 - len(codec_buf))
        except OSError:
            video_sock.close()
            control_sock.close()
            return None, "Codec ID read failed"
        if not chunk:
            video_sock.close()
            control_sock.close()
            return None, "Codec ID truncated"
        codec_buf.extend(chunk)

    session._video_sock = video_sock
    session._control_sock = control_sock
    return session, None
