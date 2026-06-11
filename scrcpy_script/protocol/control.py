"""scrcpy v4.0 control message builder (big-endian wire format)."""
import struct


class ControlType:
    INJECT_KEYCODE = 0
    INJECT_TEXT = 1
    INJECT_TOUCH = 2
    INJECT_SCROLL = 3
    BACK_OR_SCREEN_ON = 4
    SET_SCREEN_POWER_MODE = 10


class TouchAction:
    DOWN = 0
    UP = 1
    MOVE = 2


def _be16(v: int) -> bytes:
    return struct.pack(">H", v)


def _be32(v: int) -> bytes:
    return struct.pack(">I", v)


def _be64(v: int) -> bytes:
    return struct.pack(">Q", v)


def build_inject_keycode(keycode: int, action: int, repeat: int = 0) -> bytes:
    """Build INJECT_KEYCODE message (14 bytes)."""
    return (
        bytes([ControlType.INJECT_KEYCODE, action])
        + _be32(keycode)
        + _be32(repeat)
        + _be32(0)
    )


def build_inject_touch(
    action: int,
    x: int,
    y: int,
    screen_width: int,
    screen_height: int,
    pointer_id: int = 0,
    pressure: int = 0xFFFF,
) -> bytes:
    """Build INJECT_TOUCH_EVENT message (32 bytes).

    pointer_id=0 for generic finger touch.
    """
    return (
        bytes([ControlType.INJECT_TOUCH, action])
        + _be64(pointer_id)
        + _be32(x)
        + _be32(y)
        + _be16(screen_width)
        + _be16(screen_height)
        + _be16(pressure)
        + _be32(1)  # actionButton
        + _be32(1)  # buttons
    )


def build_inject_text(text: str) -> bytes:
    """Build INJECT_TEXT message (5+N bytes)."""
    data = text.encode("utf-8")
    return bytes([ControlType.INJECT_TEXT]) + _be32(len(data)) + data


def build_back_or_screen_on(action: int) -> bytes:
    """Build BACK_OR_SCREEN_ON message (2 bytes). action: 0=BACK, 1=SCREEN_ON."""
    return bytes([ControlType.BACK_OR_SCREEN_ON, action])


BACK_ACTION = 0


def build_set_screen_power_mode(mode: int) -> bytes:
    """mode: 0=OFF, 1=DOZE, 2=ON, 3=DOZE_SUSPEND"""
    return bytes([ControlType.SET_SCREEN_POWER_MODE, mode])


SCREEN_POWER_OFF = 0
SCREEN_POWER_DOZE = 1
SCREEN_POWER_ON = 2
SCREEN_POWER_DOZE_SUSPEND = 3
