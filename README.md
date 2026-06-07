# ScrcpyScript

Multi-device Android automation — Python edition. Control one or more Android devices via scrcpy v4.0 protocol, run automation scripts with OpenCV template matching, all from a DearPyGui desktop UI.

Zero C++ dependencies. Pure Python glue over C extensions (PyAV, OpenCV, numpy).

## Quick Start

```bash
# Install
pip install --break-system-packages -r requirements.txt

# Run (auto-detects all connected adb devices)
python scrcpy_script/main.py

# Headless: connect a specific device and run a script
python scrcpy_script/main.py --device <SERIAL> --script scrcpy_script/scripts/test_tap.py
```

**Prerequisites:** adb in PATH, Android device with USB debugging enabled.

## Features

| Feature | |
|---|---|
| Auto-detect & connect adb devices | Hot-plug (2s polling) |
| Real-time video preview (DearPyGui) | Per-device script threads |
| Template matching (OpenCV) | Touch/key injection (scrcpy v4.0) |
| Script Run/Stop per device | Start All / Stop All |
| Hot-reload scripts (watchdog) | Screenshot → coordinate + region picker |
| TCP wireless connection | Native scrcpy windows (subprocess) |
| Adaptive UI (width from aspect ratio, height fills viewport) | FPS counter per device |

## Project Structure

```
scrcpy_script/
├── main.py                  # Entry point, argparse, config
├── api.py                   # ScriptAPI — user-facing automation API
├── config.py                # key=value config file reader
├── scrcpy_launcher.py       # Launch native scrcpy windows
├── device/
│   ├── manager.py           # adb discovery, hot-plug, auto-connect
│   └── session.py           # Per-device: protocol + PyAV decode + frame queue
├── protocol/
│   ├── control.py           # scrcpy v4.0 control message builder (big-endian)
│   └── server.py            # Server launch, video socket, packet reader
├── script/
│   └── runner.py            # Script thread with threading.Event stop signal
├── scripts/
│   ├── test_tap.py          # Example: tap screen center every 2s
│   └── test_core.py         # Example: frame receipt + find() latency test
├── ui/
│   ├── app.py               # DearPyGui main window, toolbar
│   ├── device_card.py       # Per-device card (preview, controls, log)
│   ├── log_panel.py         # Scrollable log (deque, max 200)
│   └── region_picker.py     # Freeze frame → pick coordinates → save template
├── scrcpy_config.conf       # Default configuration
└── templates/               # Saved template images (.png)
```

17 Python files, ~1800 lines. C++ equivalent was 5508 lines (67% reduction).

## Architecture

```
Main thread (DearPyGui)
  ├── Scan / Start All / Stop All / TCP toolbar
  └── Device grid (horizontal)
       └── DeviceCard per serial
            ├── Preview (raw_texture, updated via queue.Queue)
            ├── Script combo + Run/Stop
            ├── Screenshot → RegionPicker popup
            └── Log panel

Per device (2 threads):
  Decode thread      Script thread
  ┌─────────┐       ┌──────────┐
  │ PyAV     │       │ script() │
  │  decode  │──┐    │ api.*()  │
  └─────────┘  │    └──────────┘
       │       │         │
   _cached_frame│   _stop_event
       │       │         │
       └──────[queue.Queue(maxsize=2)]──→ UI refresh
```

3 threads per device (decode, script, UI main). Hot paths in C extensions release the GIL — no bottleneck.

## Configuration

`scrcpy_config.conf` uses `key=value` format with `#` comments:

```ini
# Video
video_codec=h264
max_size=1280
bit_rate=8000000
max_fps=60

# Connection
port_start=27183
port_end=27282
max_devices=10

# Paths
server_jar=scrcpy-server-v4.0.jar
scripts_dir=scrcpy_script/scripts
templates_dir=templates
```

CLI overrides config: `--device`, `--config`, `--jar`, `--adb`.

## Writing Scripts

Scripts are Python modules with a `script(api)` function. Place them in `scripts_dir`.

```python
# scripts/my_script.py
def script(api):
    api.log(f"Running on {api.device_serial()}")
    while True:
        btn = api.find("templates/login.png", threshold=0.8)
        if btn:
            api.tap(btn["x"], btn["y"])
        api.wait(1000)
```

Key points:
- `api.wait(ms)` checks the stop event every 100ms — clicking Stop breaks the loop immediately.
- `api.find()` uses `@lru_cache` for template loading — first load is cached.
- Exceptions are caught by the runner and shown in the log panel.
- Scripts run in a daemon thread — the UI stays responsive.

See [API Reference](#api-reference) for the complete list.

## Protocol

Implements scrcpy v4.0 wire protocol directly:

1. `pkill` stale server → push `scrcpy-server-v4.0.jar` → `adb forward` two ports
2. Launch `app_process` with `tunnel_forward=true send_frame_meta=true send_dummy_byte=true`
3. Connect video socket → read dummy byte → connect control socket
4. Read device name (64 bytes) + codec ID (4 bytes)
5. PyAV `av.CodecContext("h264")` decodes raw H.264 NAL units (12-byte scrcpy header stripped, Annex B prefix added)
6. Control socket sends 14/32-byte big-endian messages for touch/key/text injection

All integers big-endian, matching scrcpy v4.0 spec.

## Dependencies

```
av>=12.0                    # PyAV — FFmpeg H.264 decode
opencv-python-headless>=4.8 # Template matching (headless: no GUI backend, saves ~50MB)
numpy>=1.24                 # Frame data arrays
dearpygui>=1.10             # Immediate-mode GPU UI
watchdog>=4.0               # Hot-reload file watching (optional)
```

## License

MIT

## Related

- [scrcpy](https://github.com/Genymobile/scrcpy) — Protocol reference
- [py-scrcpy-client](https://github.com/leng-yue/py-scrcpy-client) — Alternative Python scrcpy client (v1.24 protocol)
