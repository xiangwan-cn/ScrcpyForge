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
- Connected devices start sessions automatically; new sessions default to
  five-second preview mode.
- Desktop, template-capture browser page, CLI, and headless API workflows.
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

The daemon listens on `0.0.0.0:27180` by default so the built-in template
capture page can be opened from another device on the same LAN. Open
`http://<host-lan-ip>:27180/` in that device's browser. Set `SCRCPYFORGE_ADDR`
to restrict the listener to a specific address. The bundled browser page is
limited to template acquisition; use the desktop client, CLI, or API for other
device and script controls.

## Typical workflow

1. Start the daemon; it scans for connected devices automatically.
2. For an unpaired device, choose **Wireless pairing** in the desktop client
   or use the API and enter Android's six-digit pairing code.
3. The daemon automatically starts a scrcpy session for every connected device.
4. Preview defaults to one frame every five seconds; choose another preview or
   performance profile only when needed.
5. Run a named Lua script or submit Lua source through the API.
6. Observe script logs and lifecycle events over `/api/v1/events`.

The explicit session start endpoint remains available for retries or clients
that want to override capture options:

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
| `SCRCPYFORGE_ADDR` | `0.0.0.0:27180` | Daemon listen address; also used by the CLI. |
| `SCRCPYFORGE_API` | `http://127.0.0.1:27180/api/v1` | Desktop launch helper API URL. |
| `SCRCPYFORGE_ADB` | `adb` | ADB executable path or command name. |
| `SCRCPYFORGE_SERVER_JAR` | auto-discovered | scrcpy server v4.0 artifact. |
| `SCRCPYFORGE_DATA_DIR` | platform user-data directory | Root for mutable runtime data. |
| `SCRCPYFORGE_SCRIPTS_DIR` | `<data>/scripts` | Named Lua script packages. |
| `SCRCPYFORGE_TEMPLATES_DIR` | `<data>/templates` | Saved image regions/templates. |
| `SCRCPYFORGE_ADB_TIMEOUT_MS` | `15000` | Timeout for one ADB subprocess command. |
| `SCRCPYFORGE_MDNS_TIMEOUT_MS` | `2000` | One mDNS discovery wait, clamped to 500–4000 ms. |
| `SCRCPYFORGE_CV_THREADS` | host-dependent, max `4` by default | OpenCV matching thread budget. |
| `SCRCPYFORGE_DECODE_THREADS` | host parallelism capped at `2` | FFmpeg decoder thread count per session. |
| `SCRCPYFORGE_DECODE_THREAD_TYPE` | `slice` | FFmpeg threading mode (`slice`, `frame`, or `none`). |
| `SCRCPYFORGE_AUTH_TOKEN` | unset | Optional; when set, protects all non-public API routes with a Bearer token (at least 16 bytes). |
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
them. Static screens do not wake the frame callback unless a script explicitly
sets a rescan interval.

Declarative vision scripts scan the full frame on every callback until a target
has three nearby-position hits. They then save a device-scoped ROI sized at
twice the template width and three times its height under the named script
directory and use only that ROI. Each device gets a separate state file, so
devices cannot reuse one another's learned positions and moving the script
directory moves the learned state with it. Misses never expand the ROI or fall
back to a full scan; delete the corresponding device state file to relearn. Cooldown is disabled by default; when configured, only a
successful action starts it.
A script may opt into periodic checks of a
static latest frame without building a historical frame queue.

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

The default API listens on all interfaces for LAN template acquisition. Without
a token, browser requests carrying an unrelated `Origin` are rejected, but
direct clients on the LAN can still reach the API. Set a random
`SCRCPYFORGE_AUTH_TOKEN` of at least 16 bytes to require
`Authorization: Bearer <token>` on every non-public route. Lua runs with a
restricted standard library without `io`, `os`, `package`, or dynamic module
loading. Scripts can still control connected devices and should be managed as
privileged automation code.

## License

The Rust crates are licensed under MIT. The downloaded scrcpy server remains
subject to the upstream scrcpy license.
