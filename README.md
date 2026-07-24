# ScrcpyForge

[简体中文](README_CN.md) | English

ScrcpyForge is a cross-platform Android automation runtime built on ADB,
scrcpy 4.0, FFmpeg, and embedded Lua 5.4. A headless daemon owns every device
and media/control socket, while the desktop UI, CLI, browser UI, and other
clients use one versioned local REST/WebSocket API.

## Features

- USB and wireless-ADB discovery, including pairing-code setup and
  paired-device mDNS discovery.
- H.264, H.265, and AV1 scrcpy video sessions with latest-frame delivery.
- Desktop, browser, CLI, and headless API workflows.
- Per-device Lua automation with native template matching and input control.
- Independent script and preview performance profiles.
- Real-time JPEG WebSocket preview, five-second preview, or preview disabled.
- Portable runtime paths with no build-machine or device-specific settings.

## Requirements

- Rust stable toolchain and a C/C++ compiler.
- `adb` available on `PATH` (or set `SCRCPYFORGE_ADB`).
- FFmpeg development/runtime libraries supported by `ffmpeg-next`.
- OpenCV headers and libraries required by the native vision bridge.
- An Android device with USB debugging or wireless debugging enabled.

The exact FFmpeg/OpenCV package names vary by operating system and
distribution. The scrcpy server is not committed to the repository; fetch the
pinned official v4.0 artifact before starting a session.

## Quick start

```sh
git clone https://github.com/xiangwan-cn/ScrcpyForge.git
cd ScrcpyForge
./tools/fetch-server.sh
cargo build --release --workspace
./target/release/forge-daemon
```

In another terminal:

```sh
cargo run -p forge-cli -- scan
cargo run -p forge-cli -- devices
cargo run -p forge-desktop
```

The daemon listens on `127.0.0.1:27180` by default. Its built-in browser UI is
available at `http://127.0.0.1:27180/`.

## Typical workflow

1. Start the daemon and scan for devices.
2. For an unpaired device, choose **Wireless pairing** in the browser or
   desktop client and enter Android's six-digit pairing code.
3. Start a scrcpy session for a selected device.
4. Choose preview and performance profiles as needed.
5. Run a named Lua script or submit Lua source through the API.
6. Observe script logs and lifecycle events over `/api/v1/events`.

Example session start:

```sh
curl -X POST http://127.0.0.1:27180/api/v1/sessions/DEVICE_SERIAL/start \
  -H 'content-type: application/json' \
  -d '{"codec":"h264","max_size":1280,"bit_rate":8000000,"max_fps":60}'
```

See [API.md](API.md) for every endpoint and request shape. See
[LUA_API.md](LUA_API.md) and `scripts/example_all_api/script.lua` for Lua
automation.

## Components

- `forge-core`: ADB discovery, session lifecycle, decoding, vision, control,
  and Lua runtime.
- `forge-daemon`: REST/WebSocket backend and integration boundary.
- `forge-cli`: portable command-line API client.
- `forge-desktop`: standalone desktop client.

The daemon is the only owner of ADB and scrcpy sockets. Clients can restart or
coexist without terminating active device sessions.

## Configuration

| Variable | Default | Purpose |
| --- | --- | --- |
| `SCRCPYFORGE_ADDR` | `127.0.0.1:27180` | Daemon listen address; also used by the CLI. |
| `SCRCPYFORGE_API` | `http://127.0.0.1:27180/api/v1` | Desktop launch helper API URL. |
| `SCRCPYFORGE_ADB` | `adb` | ADB executable path or command name. |
| `SCRCPYFORGE_SERVER_JAR` | auto-discovered | scrcpy server v4.0 artifact. |
| `SCRCPYFORGE_DATA_DIR` | platform user-data directory | Root for mutable runtime data. |
| `SCRCPYFORGE_SCRIPTS_DIR` | `<data>/scripts` | Named Lua script packages. |
| `SCRCPYFORGE_TEMPLATES_DIR` | `<data>/templates` | Saved image regions/templates. |
| `SCRCPYFORGE_FONT` | platform CJK font discovery | Optional UI font file. |
| `RUST_LOG` | daemon info logs | Standard tracing filter. |

Portable bundles may place `scripts/`, `templates/`, and
`resources/scrcpy-server-v4.0.jar` beside the executable. The repository
ignores user scripts and their image assets except for the portable API example.

## Automation and performance

Each device has an independent Lua state. Frame delivery uses a replaceable
latest-frame slot: if a callback is slower than the video stream, old pending
frames are discarded instead of accumulating latency. Frames stay in I420 after
decoding; RGB and JPEG are generated lazily only when vision or preview needs
them.

Script profiles (`auto`, `eco`, `balanced`, `realtime`) and preview profiles are
configured independently. Performance depends on the host, encoder, transport,
frame dimensions, templates, and scene; no model-specific tuning is selected at
compile time or runtime.

## Documentation

- [Local API v1](API.md)
- [Lua API](LUA_API.md)
- [Architecture](ARCHITECTURE.md)
- [Performance validation](PERFORMANCE.md)

## Security

The API currently has no authentication and uses permissive CORS. Keep the
default loopback bind unless the surrounding network and access controls are
trusted. Lua scripts can control connected devices and should be treated as
trusted local code.

## License

The Rust crates are licensed under MIT. The downloaded scrcpy server remains
subject to the upstream scrcpy license.
