# ScriptAPI Reference

The `ScriptAPI` object is passed to your `script(api)` function. All methods are synchronous and thread-safe.

## Constants

```python
api.KEY_BACK = 4          api.KEY_HOME = 3
api.KEY_ENTER = 66        api.KEY_POWER = 26
api.KEY_VOLUME_UP = 24    api.KEY_VOLUME_DOWN = 25
api.KEY_MENU = 82
```

## Frame Access

### `capture() -> np.ndarray | None`
Return the current cached frame as a BGR numpy array (H×W×3). Returns `None` if no frame has been received yet.

```python
frame = api.capture()
if frame is not None:
    h, w = frame.shape[:2]
```

### `screen_size() -> tuple[int, int]`
Device screen resolution (width, height). Updated on session packets (rotation).

### `video_size() -> tuple[int, int]`
Video stream resolution. May differ from screen_size if `max_size` scaling is active.

## Template Matching

Templates are `.png` files placed in `templates/` (configurable). Loaded with `@lru_cache(maxsize=32)` — repeated calls reuse cached images.

### `find(tpl_path: str, threshold: float = 0.8) -> dict | None`
Find a single template match using `cv2.matchTemplate` with `TM_CCOEFF_NORMED`. Returns the best match above threshold, or `None`.

Return value: `{"x": center_x, "y": center_y, "w": width, "h": height, "confidence": 0.92}`

```python
btn = api.find("templates/login_btn.png", threshold=0.75)
if btn:
    api.tap(btn["x"], btn["y"])
```

### `find_all(tpl_path: str, threshold: float = 0.8) -> list[dict]`
Find all matches above threshold. Returns a list of dicts (same format as `find()`).

```python
for match in api.find_all("templates/icon.png"):
    api.log(f"Found at ({match['x']}, {match['y']})")
```

### `wait_for(tpl_path: str, timeout_ms: int = 10000) -> dict | None`
Poll `find()` every 50ms until match found or timeout. Returns match dict or `None`. Checks stop event — clicking Stop breaks the wait.

```python
btn = api.wait_for("templates/loading.png", timeout_ms=30000)
if btn is None:
    api.warn("Loading screen not found")
```

## Touch (Device Coordinates)

All coordinates are device screen coordinates (not video stream). The scrcpy server normalizes them using `screen_width`/`screen_height` in the control message.

### `tap(x: int, y: int)`
Inject a touch-down-then-up at the given position.

```python
api.tap(540, 1200)  # center of 1080×2400 screen
```

### `swipe(x1: int, y1: int, x2: int, y2: int, duration_ms: int = 300)`
Swipe from (x1, y1) to (x2, y2) with interpolation steps every 16ms.

```python
api.swipe(540, 1800, 540, 600, duration_ms=500)  # swipe up
```

### `long_press(x: int, y: int, duration_ms: int)`
Touch down, wait, touch up.

### `multi_tap(points: list[tuple[int, int]])`
Simultaneous multi-point touch (all down, then all up).

```python
api.multi_tap([(200, 300), (800, 300)])  # two-finger tap
```

## Input

### `press_key(keycode: int, long_press: bool = False)`
Inject an Android keycode. Use the `KEY_*` constants.

```python
api.press_key(api.KEY_HOME)
api.press_key(api.KEY_POWER, long_press=True)
```

### `press_back()`
Convenience: press the Android BACK button.

### `input_text(text: str)`
Inject UTF-8 text. Maximum ~300 bytes per protocol limit.

```python
api.input_text("hello@example.com")
```

## Flow Control

### `wait(ms: int)`
Sleep for `ms` milliseconds. Checks stop event every 100ms — raises `StopIteration` if Stop was clicked, which exits the script cleanly.

```python
api.wait(2000)  # 2 seconds
```

### `repeat_until(fn: Callable[[], bool], timeout_ms: int) -> bool`
Call `fn()` every 50ms until it returns `True` or timeout. Returns success boolean.

```python
def is_screen_ready():
    return api.find("templates/ready.png") is not None

if api.repeat_until(is_screen_ready, 10000):
    api.log("Screen ready")
```

## Output

### `log(msg: str)`
Write an info message to the device's log panel.

### `warn(msg: str)`
Write a warning to the log panel.

### `screenshot(path: str)`
Save the current cached frame as a PNG file.

```python
api.screenshot("debug/capture.png")
```

### `device_serial() -> str`
Return the device's adb serial.

### `device_name() -> str`
Return the device name from scrcpy handshake (e.g., "21051182C").

### `is_rotated() -> bool`
True if the device screen has rotated from its initial orientation.

## Script Lifecycle

```
┌──────────────────────────────────────┐
│ ScriptRunner.run()                   │
│  api.log("Script started")           │
│  try:                                │
│    script(api)  ← your code          │
│  except StopIteration:               │
│    pass  # Normal stop               │
│  except Exception as e:              │
│    api.warn(f"Script error: {e}")   │
│  finally:                            │
│    api.log("Script stopped")         │
└──────────────────────────────────────┘
```

- Clicking **Run** creates `ScriptAPI` + `ScriptRunner` in a new daemon thread.
- Clicking **Stop** sets `threading.Event` → `api.wait()` raises `StopIteration` → script exits.
- Script crash only affects that device — others keep running.
- Hot-reload (watchdog): edit a `.py` script file → script list refreshes. Manually Stop/Start to pick up changes.

## Complete Example

```python
"""Auto-login script with retry."""
def script(api):
    api.log(f"Device: {api.device_serial()} ({api.device_name()})")

    for attempt in range(1, 4):
        api.log(f"Attempt {attempt}")

        # Wait for login button
        btn = api.wait_for("templates/login_btn.png", timeout_ms=10000)
        if btn is None:
            api.warn("Login button not found")
            continue

        api.tap(btn["x"], btn["y"])
        api.wait(500)

        # Enter password
        api.input_text("password123")
        api.wait(300)

        # Tap confirm
        api.tap(btn["x"], btn["y"] + 200)
        api.wait(2000)

        # Check if logged in
        if api.find("templates/logged_in.png"):
            api.log("Login successful")
            break
    else:
        api.warn("Login failed after 3 attempts")
```
