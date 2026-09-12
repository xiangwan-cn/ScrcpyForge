# Performance validation

Performance depends on host CPU, device encoder, transport, frame dimensions,
template size, and scene complexity. Published results should therefore record
all of those variables instead of treating one device as a universal baseline.

For a reproducible recognition ceiling test:

1. Put the benchmark and its template in a local ignored script directory.
2. Select the `realtime` script profile and turn preview off.
3. Use single-scale, full-screen matching without cooldown or ROI tracking.
4. Record decoded/script FPS, mean and P95 match time, dropped frames, control
   dispatch time, and end-to-end visual response separately.
5. Repeat with color and I420 gray matching when the target does not require
   color information.

Production scripts should normally use the `auto` profile. Prefer ROI tracking
when the workflow permits it, and enable preview only when an operator needs it;
preview JPEG encoding is intentionally independent from script processing.

The desktop client marks only cards intersecting the scroll viewport as preview
consumers. Scrolling a card away releases its WebSocket lease and prevents its
five-second screenshot request until it returns; a hidden or unfocused window
releases every lease.

When all consumers release their leases, a session remains warm briefly and
then enters `suspended`: encoded packets are discarded before FFmpeg and no
RGB/JPEG work runs. After the deep-idle window the daemon stops the scrcpy
server and its ADB forwards. A new demand reactivates a warm session; for
connected devices, the daemon's background scan recreates a dead session
automatically. Measure warm versus cold resume separately when tuning this
tradeoff.

The runtime exposes `SCRCPYFORGE_CV_THREADS` and
`SCRCPYFORGE_DECODE_THREADS`/`SCRCPYFORGE_DECODE_THREAD_TYPE` so a multi-device
host can keep OpenCV and FFmpeg within one CPU budget. `SCRCPYFORGE_ADB_TIMEOUT_MS`
puts a bound on control-plane subprocesses. `/api/v1/state` contains stable
session state, while `/api/v1/metrics` contains rolling counters. Compare
`latest_frame_age_ms`, `last_publish_us`, `script_rescans`, `input_failures`,
and video error counters before tuning a matching threshold.

For energy measurements, hold device brightness, network, temperature, and
scene constant. Compare ten-minute windows with no consumers, a hidden Web
page, and an active script. On Linux record CPU time and the powercap
`energy_uj` delta when available; on Android record the scrcpy/MediaCodec
process state and a fixed Perfetto or batterystats window. Report repeated runs
with the exact profile and frame dimensions instead of inferring savings from
FPS alone.

For tracking A/B runs, compare full-screen matching with tracking enabled on the
same captured frames. Include a static screen after script attach, a target move
of 10/50/150 pixels, a two-frame local miss, a geometry change, and a target
that remains visible after a tap. Accept the ROI path only when true hits and
false taps are no worse than the full-screen baseline and the recovery counters
show the expected local → expanded → full sequence.
