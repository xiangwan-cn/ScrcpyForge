"""Template match and click with random 5px offset."""
import random


# TODO: replace with your template image path
TEMPLATE = "templates/your_template.png"
# TODO: adjust threshold if needed (0.0–1.0, higher = stricter)
THRESHOLD = 0.8


def script(api):
    api.log(f"Matching: {TEMPLATE} (threshold={THRESHOLD})")

    while True:
        match = api.find(TEMPLATE, threshold=THRESHOLD)
        if match:
            ox = random.randint(-5, 5)
            oy = random.randint(-5, 5)
            tx, ty = match["x"] + ox, match["y"] + oy
            api.tap(tx, ty)
            api.log(
                f"Match at ({match['x']},{match['y']}) "
                f"conf={match['confidence']:.2f} "
                f"→ tapped ({tx},{ty})"
            )
            api.wait(1500)
        else:
            api.wait(200)
