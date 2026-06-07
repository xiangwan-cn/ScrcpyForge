"""Phase 1 verification: core pipeline latency test."""
import time


def script(api):
    api.log(f"Device: {api.device_serial()} ({api.device_name()})")
    api.log(f"Screen: {api.screen_size()}, Video: {api.video_size()}")

    last_frame = 0.0
    frame_count = 0
    start = time.monotonic()

    while True:
        frame = api.capture()
        if frame is not None:
            now = time.monotonic()
            if last_frame > 0:
                interval = (now - last_frame) * 1000
                api.log(f"Frame interval: {interval:.1f}ms")
            last_frame = now
            frame_count += 1

        t0 = time.monotonic()
        result = api.find("templates/test.png", threshold=0.5)
        t1 = time.monotonic()
        if result:
            api.log(f"find()={t1-t0:.3f}ms  match at ({result['x']},{result['y']})")

        api.wait(2000)

        if frame_count >= 5:
            api.log(f"Phase 1 complete: {frame_count} frames in {time.monotonic()-start:.1f}s")
            break
