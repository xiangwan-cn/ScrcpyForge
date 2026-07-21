# Local API v1

Default base URL: `http://127.0.0.1:27180/api/v1`

All request and response bodies are JSON unless a route states otherwise.
Serials are path parameters and must be URL-encoded (wireless and IPv6 serials
can contain `:`, `[` and `]`). Successful action-only responses generally use
`204 No Content` or `202 Accepted`.

Errors produced by handlers have status `500` and the form:

```json
{"error":"human-readable message"}
```

The API currently has no authentication and permits CORS from any origin. Keep
the daemon bound to loopback unless it is protected by a trusted access layer.

## Service

### `GET /health`

Returns service state and crate version.

```json
{"status":"ok","version":"0.1.0"}
```

### `GET /capabilities`

Returns the API version, scrcpy server version, codecs, vision modes, Lua
version, preview modes, and performance profiles supported by this daemon.

### `POST /shutdown`

Requests graceful daemon shutdown. Returns `202 Accepted`.

## Devices

### `GET /devices`

Returns the most recent device snapshot. The daemon refreshes it every five
seconds.

```json
[
  {
    "serial":"DEVICE_SERIAL",
    "state":"device",
    "product":"product_name",
    "model":"model_name",
    "transport_id":"1",
    "wireless":false
  }
]
```

`state` is `device`, `offline`, `unauthorized`, or `unknown`. Optional ADB
metadata fields may be `null`.

### `POST /devices/scan`

Refreshes ADB devices, tries paired wireless-debugging mDNS endpoints when
appropriate, and returns the new device array.

### `POST /devices/connect`

Connects a reachable wireless ADB endpoint, rescans, and returns the device
array.

```json
{"endpoint":"192.168.1.10:5555"}
```

### `GET /devices/{serial}/screenshot`

Returns an ADB screenshot as `image/png`. This route does not require a running
scrcpy session.

### `POST /devices/{serial}/input`

Sends one input action. Tap and text use the scrcpy control socket when a
session exists; other actions have an ADB fallback. Returns `204 No Content`.

```json
{"type":"tap","x":320,"y":640}
```

```json
{"type":"swipe","x1":100,"y1":500,"x2":100,"y2":100,"duration_ms":300}
```

```json
{"type":"text","value":"hello world"}
```

```json
{"type":"key","code":4}
```

## Sessions

A session launches the pinned scrcpy server on one device, establishes video
and control sockets, and starts FFmpeg decoding.

### `GET /sessions`

Returns the serials of running sessions.

```json
["DEVICE_SERIAL"]
```

### `POST /sessions/{serial}/start`

All fields are optional. Unknown codec strings currently fall back to `h264`.

```json
{
  "server_jar":"/optional/path/scrcpy-server-v4.0.jar",
  "codec":"h264",
  "max_size":1280,
  "bit_rate":8000000,
  "max_fps":60
}
```

Defaults are H.264, maximum dimension 1280, 8 Mbps, 60 fps, automatic encoder,
and stay-awake enabled. Starting an already-running session is idempotent.

```json
{"device_name":"Android device","codec":"H264"}
```

### `POST /sessions/{serial}/stop`

Stops the device script first, then shuts down the session and its ADB forward.
Returns `204`, or `404` when no session exists.

### `POST /sessions/start-all`

Starts default sessions concurrently for every device in the current snapshot.

```json
{"started":["SERIAL_A"],"failed":{"SERIAL_B":"error message"}}
```

### `POST /sessions/stop-all`

Stops scripts and sessions for all current devices. Returns `204 No Content`.

### `POST /sessions/{serial}/preview-mode`

```json
{"mode":"realtime"}
```

Modes are `realtime`, `five_seconds`, and `off`. An unknown value currently
falls back to `realtime`. Returns `204 No Content`.

### `POST /sessions/{serial}/script-profile`

Sets Lua/frame-delivery performance independently of preview.

```json
{"profile":"auto"}
```

Profiles are `auto`, `eco`, `balanced`, and `realtime`. Returns `204`.
`POST /sessions/{serial}/profile` is a compatibility alias for this endpoint.

### `POST /sessions/{serial}/preview-profile`

Sets JPEG preview performance independently of scripts. It accepts the same
profile body and returns `204 No Content`.

### `GET /sessions/{serial}/metrics`

Returns cumulative counts and rolling performance data.

```json
{
  "decoded_frames":1200,
  "preview_frames":150,
  "script_frames":420,
  "dropped_script_frames":12,
  "decoded_fps":59.8,
  "preview_fps":10.0,
  "script_fps":30.0,
  "latest_frame_age_ms":8.2,
  "average_script_ms":3.4,
  "script_p50_ms":3.0,
  "script_p95_ms":5.8,
  "profile":"auto",
  "preview_profile":"eco"
}
```

### `GET /sessions/{serial}/frame.jpg`

Returns the latest decoded frame as `image/jpeg` with `Cache-Control: no-store`.
The session must have decoded at least one frame.

### `GET /sessions/{serial}/preview`

Upgrades to WebSocket. Every binary message is one complete JPEG image. Frame
frequency follows the preview mode/profile; slow clients may skip frames.

### `POST /sessions/{serial}/regions`

Crops the latest decoded frame and saves a PNG. Coordinates are
`x1,y1,x2,y2`. Supply either a safe template `name` or `path`; a relative path
is resolved under the templates directory.

```json
{"name":"confirm_button","x1":100,"y1":200,"x2":300,"y2":260}
```

```json
{
  "path":"/resolved/templates/confirm_button.png",
  "serial":"DEVICE_SERIAL",
  "region":[100,200,300,260]
}
```

## Scripts

Only one frame-driven script is active per device. Starting another script for
the same serial cancels and replaces the previous one. Named scripts use
`scripts/<name>/script.lua`; names may contain ASCII letters, digits, `-`, and
`_` only.

### `GET /scripts`

Returns sorted names of valid script directories.

```json
["example_all_api"]
```

### `POST /scripts/run`

Runs submitted Lua source on a session that is already active. The optional
`name` also selects that named script directory as the trusted `forge.asset()`
root; omit it for anonymous source.

```json
{"serial":"DEVICE_SERIAL","source":"function on_frame(frame) end","name":null}
```

Returns `202 Accepted`:

```json
{"run_id":"550e8400-e29b-41d4-a716-446655440000"}
```

### `POST /scripts/run-named`

Loads the current named file contents and runs it on one active session.

```json
{"serial":"DEVICE_SERIAL","name":"example_all_api"}
```

Returns the same `202` response as `/scripts/run`.

### `POST /scripts/run-all`

Runs one named script on every currently active session.

```json
{"name":"example_all_api"}
```

```json
{"run_ids":["550e8400-e29b-41d4-a716-446655440000"]}
```

### `GET /scripts/runs`

Returns tracked script runs and their health.

```json
[
  {
    "run_id":"550e8400-e29b-41d4-a716-446655440000",
    "serial":"DEVICE_SERIAL",
    "name":"example_all_api",
    "running":true,
    "stalled":false,
    "error":null
  }
]
```

### `POST /scripts/{run_id}/stop`

Cancels one run. Returns `202`, or `404` when the run is not tracked.

### `POST /scripts/devices/{serial}/stop`

Cancels the active script for a device. Returns `202`, or `404` if absent.

### `POST /scripts/stop-all`

Cancels all tracked scripts and returns `202 Accepted`.

## Events

### `GET /events`

Upgrades to WebSocket. Each text message contains one tagged JSON event:

```json
{"type":"device_snapshot","devices":[]}
```

```json
{"type":"script_started","run_id":"...","serial":"DEVICE_SERIAL"}
```

```json
{"type":"script_log","run_id":"...","message":"matched"}
```

```json
{"type":"script_stopped","run_id":"...","error":null}
```

Lagging event clients skip old messages and continue with new events.

## Versioning

The `/api/v1` prefix is the compatibility boundary between the daemon and its
independently updated clients. Additive fields may appear within v1; clients
should ignore fields they do not recognize.
