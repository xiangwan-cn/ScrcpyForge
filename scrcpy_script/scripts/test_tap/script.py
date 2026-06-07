"""Test script: tap screen center every 2 seconds."""


def script(api):
    w, h = api.screen_size()
    cx, cy = w // 2, h // 2
    api.log(f"Screen: {w}x{h}, tapping center ({cx}, {cy})")

    while True:
        api.tap(cx, cy)
        api.log(f"Tap at ({cx}, {cy})")
        api.wait(2000)
