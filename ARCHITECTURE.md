# Architecture

```text
f-mon page / standalone UI / CLI
              |
       REST + WebSocket API
              |
          forge-daemon
              |
   device manager + event bus
     /          |          \
   ADB     scrcpy session   Lua runtime
             /       \
       video backend  control socket
```

The daemon is the single owner of devices and scrcpy sessions. Frontends can be
restarted independently and multiple clients can observe the same session.

Each device owns a bounded runtime: the async socket reader only parses packets,
a named decoder worker owns FFmpeg, a latest-frame slot feeds one Lua worker,
and a bounded InputWriter serializes control messages. The script slot is
latest-only and injects the newest already-decoded frame when a run attaches;
the worker can recheck that frame at a configured low rate when scrcpy has no
new packet; the default path waits for a new frame or cancellation without a
timer.
Session shutdown is explicit and idempotent; it terminates the server, input
writer, frame delivery and ADB forwards even while clients still hold session
references.

Frame demand has three states: `active` while a script or preview lease exists,
`idle_grace` for a short warm period, and `suspended` after demand disappears.
Suspended sessions keep the logical entry and cached codec state while dropping
media before FFmpeg; a later demand can inject the cached keyframe even when a
static device has not emitted a new packet. After the deep-idle limit they
terminate the scrcpy child and forwards, and the manager removes the dead
session. The daemon's connected-device scan starts a fresh session
automatically when the device remains available. New sessions use the
five-second preview mode; an explicit preview or script lease still wakes
decoding immediately.

## Cross-platform boundary

All process invocation uses argument arrays, never a platform shell. Runtime
paths come from configuration. The current portable decoder uses FFmpeg. Frames
remain in compact I420 after decoding; RGB and JPEG are generated lazily and
cached only when vision or a preview client requests them. This avoids a full
color conversion on every frame when preview is disabled.

## Lua

Lua 5.4 is embedded with a vendored runtime. Scripts receive a `forge` table:

- `forge.serial()`
- `forge.log(message)`
- `forge.wait(milliseconds)`
- `forge.tap(x, y, radius)`
- `forge.swipe(x1, y1, x2, y2, duration_ms)`
- `forge.text(value)`
- `forge.key(code)`
- `frame:find`, `frame:find_fast`, `frame:find_first`, `frame:find_candidates`, `frame:find_multiscale`
- `frame:pixel`, `frame:save`, `frame:crop`

Cancellation is cooperative and checked by every API call and wait interval.
`forge.wait` remains available for ordinary sequential scripts; frame-driven
vision policies use per-target cooldowns so a callback never sleeps for a
business delay. Computer-vision functions remain native Rust/OpenCV operations
exposed to Lua, so image processing does not execute in the interpreter.

`forge.vision.compile` builds a reusable policy plan. Native template caches are
invalidated when a template file is overwritten. Color matching is available
for color-sensitive targets, while `find_gray` and `find_fast_gray` operate
directly on the I420 Y plane and avoid full-frame RGB conversion. Color searches
can convert only an even-aligned ROI. Candidates return one best result per
template, so the policy layer can compare targets without priority-order early
return. Declarative vision has two search paths: full-frame matching until
three nearby-position hits establish a persistent ROI, then matching only
inside that ROI. The saved ROI is device-scoped, twice the template width by
three times its height; misses do not expand it or fall back to a full-screen
search. Devices using the same script keep separate state files.

Frame-driven scripts define `on_frame(frame)`. Delivery is a single replaceable
slot: while a callback runs, older pending frames are discarded and the next
callback always receives the newest frame. This bounds latency and memory.
Preview has its own performance profile and lossy WebSocket channel, independent
from script delivery. A `VideoFrame` caches its JPEG with a single initialization
path so multiple preview clients share the encoded bytes. `five_seconds` emits
one preview every five seconds, and `off` performs no preview JPEG encoding.
The desktop frontend reports the cards intersecting its scroll viewport; the
backend keeps realtime sockets and five-second JPEG requests only for those
cards while the window is visible.
The daemon reports rolling five-second FPS, recent P50/P95 callback latency,
latest-frame sequence/rescans, input completion timing, publish timing, video
errors and dropped-frame counts through the aggregate `/api/v1/metrics` route
and the per-session metrics route. Stable session state stays in `/state`, so
rolling counters do not invalidate its ETag.
Lua has a 64 MiB memory limit, instruction-level cooperative cancellation and a
five-second cooperative callback deadline. Native OpenCV and blocking host
calls are not preemptible until they return; script profiles control actual
frame delivery and `auto` adapts its interval to recent callback cost.
