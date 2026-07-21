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
