# Local API v1

Default local base URL: `http://127.0.0.1:27180/api/v1`. For LAN clients,
replace the host with the daemon's LAN address.

All request and response bodies are JSON unless a route states otherwise.
Serials are path parameters and must be URL-encoded (wireless and IPv6 serials
can contain `:`, `[` and `]`). Successful action-only responses generally use
`204 No Content` or `202 Accepted`.

Errors use an HTTP status and the form:

```json
{"error":{"code":"invalid_request","message":"human-readable message","request_id":"…"}}
```

Every response carries an `X-Request-Id` header. Include it when reporting a
failure so the matching server log entry can be located without exposing
request bodies or credentials.

`400` is used for invalid input, `404` for a missing device/session/script,
`409` for a state conflict, `429` for a busy or rate-limited request, and
`500` for an unexpected server error (whose response message is deliberately
generic; details are logged server-side). The daemon accepts at most 512 KiB per
request body; scripts are limited to 512 KiB, text input to 4 KiB, swipe and
long-press duration to 60 seconds, tap randomization radius to 4096 pixels,
and multi-touch to 10 pointers. Named script directories are limited to 128
bytes and template paths to 1024 bytes.

The default listener is `0.0.0.0:27180` for LAN template acquisition. Without a
token, a browser `Origin` that does not match the request host is rejected, but
direct LAN clients can still call the API. Set `SCRCPYFORGE_AUTH_TOKEN` to a
random value of at least 16 bytes to require `Authorization: Bearer <token>` on
every non-public route. The health and root UI routes remain public; there is no
wildcard CORS policy.

## Service

### `GET /health`

Returns service state and crate version.

```json
{"status":"ok","version":"0.1.0"}
```

### `GET /capabilities`

Returns the API version, scrcpy server version, codecs, vision modes, Lua
version, preview modes, capture profiles, ETag support, and performance
profiles supported by this daemon.

### `GET /state`

Returns one consistent snapshot for devices, running sessions, active script
runs, and available named scripts. Session entries contain only stable state,
including the current preview mode;
rolling counters and frame identity are served by the aggregate `/metrics`
endpoint. Desktop and browser clients should prefer these two aggregate
endpoints over issuing one request per device during refresh. The response
includes an `ETag`; send it as `If-None-Match` to receive `304 Not Modified`
when the stable state has not changed.

```json
{
  "devices": [],
  "sessions": [],
  "runs": [],
  "scripts": ["example_all_api"]
}
```

### `GET /metrics`

Returns the current metrics for every tracked session as an object keyed by
serial. This representation is intentionally separate from `/state`, so its
rolling FPS, frame sequence, and counters can update without invalidating the
stable state ETag.

```json
{"DEVICE_SERIAL":{"decoded_fps":59.8,"latest_frame_seq":1200,"activity_state":"active"}}
```

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

### `GET /devices/pairing-services`

Discovers Android wireless-debugging pairing services advertised as
`_adb-tls-pairing._tcp`. The response can be empty when mDNS is unavailable;
clients should allow manual entry of the pairing endpoint shown by Android.

```json
[
  {"name":"adb-123456","endpoint":"192.168.1.10:37123"}
]
```

### `POST /devices/pair`

Pairs with the endpoint shown by Android's “Pair device with pairing code”
dialog. The code must contain exactly six digits. The daemon sends it to
`adb pair` over standard input, never as a command-line argument.

```json
{"endpoint":"192.168.1.10:37123","code":"123456"}
```

After pairing, the daemon waits for ADB's automatic connection. If necessary,
it discovers the separate `_adb-tls-connect._tcp` endpoint for the same host,
connects it, and returns the refreshed device array. Pairing and connection
ports are not interchangeable.

### `GET /devices/{serial}/screenshot`

Returns an ADB screenshot as `image/png`. This route does not require a running
scrcpy session.

### `POST /devices/{serial}/input`

Sends one input action. Tap, swipe, text, and key use the scrcpy control socket
when a session exists; ADB is used only when no session is running. Returns
`204 No Content`.

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

All fields are optional. Unknown codec strings return `400`.

```json
{
  "codec":"h264",
  "max_size":1280,
  "bit_rate":8000000,
  "max_fps":60
}
```

The server jar is selected by daemon configuration (`SCRCPYFORGE_SERVER_JAR`)
and cannot be supplied by a request. Defaults are H.264, maximum dimension
1280, 8 Mbps, 60 fps, automatic encoder, and stay-awake disabled. `profile`
may be `eco`, `balanced`, or `realtime` to
select the corresponding capture defaults; explicitly supplied size, bitrate,
or FPS values take precedence. Starting an already-running session is
idempotent. The daemon automatically starts a default session when a connected
device is discovered; this endpoint remains available for an explicit retry or
capture override. New sessions use `five_seconds` preview mode by default.

```json
{"device_name":"Android device","codec":"H264"}
```

### `POST /sessions/{serial}/stop`

Stops the device script first, then shuts down the session and its ADB forward.
Returns `204`, or `404` when no session exists.

### `POST /sessions/start-all`

Starts default sessions concurrently for every device in the current snapshot.
Connected devices are normally started automatically; this endpoint is useful
for an explicit retry after a manual stop or a startup failure.

```json
{"started":["SERIAL_A"],"failed":{"SERIAL_B":"error message"}}
```

### `POST /sessions/stop-all`

Stops every tracked script and session, including sessions whose device has
already disappeared from the latest ADB scan. Returns `204 No Content`.

### `POST /sessions/{serial}/preview-mode`

```json
{"mode":"realtime"}
```

Modes are `realtime`, `five_seconds`, and `off`. An unknown value returns `400`.
Returns `204 No Content`.

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

Returns cumulative counts and rolling performance data. `latest_frame_seq` is
the decoder sequence used by frame-driven scripts; `preview_leases` reports
active WebSocket/HTTP preview consumers, and `activity_state` is `active`,
`idle_grace`, or `suspended`.

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
  "script_published":450,
  "script_rescans":30,
  "script_source_seq":1180,
  "script_generation":2,
  "last_publish_us":180,
  "input_batches":14,
  "input_failures":0,
  "last_input_us":2400,
  "video_packet_errors":0,
  "video_decode_errors":0,
  "video_dimension_changes":1,
  "last_video_error":null,
  "latest_frame_seq":1200,
  "preview_leases":1,
  "preview_dropped_frames":0,
  "script_active":true,
  "activity_state":"active",
  "idle_for_ms":0,
  "profile":"auto",
  "preview_profile":"eco"
}
```

### `GET /sessions/{serial}/frame.jpg`

Returns the latest decoded frame as `image/jpeg` with an ETag derived from the
session and `frame_seq`. Send `If-None-Match` to avoid re-encoding an unchanged
frame and receive `304 Not Modified`. A request temporarily keeps frame demand
active, including when preview mode is `off`, and waits up to three seconds for
a fresh decoded frame after a suspended session resumes.

### `GET /sessions/{serial}/preview`

Upgrades to WebSocket. Every binary message is one complete JPEG image. Frame
frequency follows the preview mode/profile; slow clients may skip frames. The
daemon limits preview and event WebSockets to 64 concurrent connections, uses
a 5-second write timeout, and rejects event messages larger than 64 KiB or
preview JPEGs larger than 8 MiB. The bundled browser page uses the still-frame
route only while acquiring a template and does not keep preview WebSockets open.

### `POST /sessions/{serial}/regions`

Crops the latest decoded frame and saves a PNG. The capture request temporarily
keeps frame demand active even when preview mode is `off`. Coordinates are
`x1,y1,x2,y2`. Supply either a safe template `name` or `path`; `path` must be a
relative `.png` path under the templates directory. Absolute paths, parent
components, symbolic-link parents, and symbolic-link outputs are rejected.

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
{"run_ids":["550e8400-e29b-41d4-a716-446655440000"],"failed":{}}
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
