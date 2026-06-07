"""Template match and click with random 5px offset, ROI-constrained."""
import random
import time


# TODO: replace with your template image path
TEMPLATE = "templates/template.png"
# TODO: adjust threshold if needed (0.0–1.0, higher = stricter)
THRESHOLD = 0.8
# ROI: (x1, y1, x2, y2) — only search within this region
ROI = (452, 248, 857, 652)


def script(api):
    api.log(
        f"Matching: {TEMPLATE} "
        f"threshold={THRESHOLD} roi={ROI}"
    )

    while True:
        t0 = time.perf_counter()
        match = api.find(TEMPLATE, threshold=THRESHOLD, roi=ROI)
        elapsed = (time.perf_counter() - t0) * 1000

        if match:
            ox = random.randint(-5, 5)
            oy = random.randint(-5, 5)
            tx, ty = match["x"] + ox, match["y"] + oy
            api.tap(tx, ty)
            api.log(
                f"({match['x']},{match['y']}) "
                f"conf={match['confidence']:.2f} "
                f"{elapsed:.1f}ms "
                f"→ tapped ({tx},{ty})"
            )
            api.wait(1500)
        else:
            api.wait(200)
