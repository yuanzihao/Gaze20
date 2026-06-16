from pathlib import Path

from PIL import Image, ImageFilter


ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "eyes-redness-score-image"
TARGET = ROOT / "public" / "eyes"

# The source PNGs include a baked checkerboard. Crop to the useful eye band and
# make only very bright, near-neutral pixels transparent so eye whites stay intact.
CROP_BOX = (110, 210, 1562, 748)
OUTPUT_SIZE = (980, 360)


def checker_to_alpha(image: Image.Image) -> Image.Image:
    rgba = image.convert("RGBA")
    pixels = rgba.load()
    width, height = rgba.size

    alpha = Image.new("L", rgba.size, 255)
    alpha_pixels = alpha.load()

    def is_checker(x: int, y: int) -> bool:
        r, g, b, _ = pixels[x, y]
        bright = min(r, g, b) >= 232
        neutral = max(r, g, b) - min(r, g, b) <= 8
        return bright and neutral

    visited = bytearray(width * height)
    queue: list[tuple[int, int]] = []

    def push(x: int, y: int) -> None:
        if x < 0 or x >= width or y < 0 or y >= height:
            return
        idx = y * width + x
        if visited[idx] or not is_checker(x, y):
            return
        visited[idx] = 1
        queue.append((x, y))

    for x in range(width):
        push(x, 0)
        push(x, height - 1)
    for y in range(height):
        push(0, y)
        push(width - 1, y)

    while queue:
        x, y = queue.pop()
        alpha_pixels[x, y] = 0
        push(x + 1, y)
        push(x - 1, y)
        push(x, y + 1)
        push(x, y - 1)

    alpha = alpha.filter(ImageFilter.GaussianBlur(radius=0.55))
    rgba.putalpha(alpha)
    return rgba


def main() -> None:
    TARGET.mkdir(parents=True, exist_ok=True)
    for old in TARGET.glob("eyes-redness-score-*.*"):
        old.unlink()

    for source in sorted(SOURCE.glob("eyes-redness-score-*.png")):
        score = source.stem.rsplit("-", 1)[-1]
        image = Image.open(source).crop(CROP_BOX)
        image = checker_to_alpha(image)
        image = image.resize(OUTPUT_SIZE, Image.Resampling.LANCZOS)
        target = TARGET / f"eyes-redness-score-{score}.webp"
        image.save(target, "WEBP", quality=78, method=6, lossless=False)
        print(f"{source.name} -> {target.relative_to(ROOT)} {target.stat().st_size / 1024:.1f} KB")


if __name__ == "__main__":
    main()
