"""Generate the Tauri app icon (multi-size .ico) without external assets."""
import pathlib

from PIL import Image, ImageDraw

SIZE = 256
img = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
d = ImageDraw.Draw(img)

# Rounded square background.
d.rounded_rectangle([8, 8, SIZE - 8, SIZE - 8], radius=52, fill=(76, 141, 255, 255))

# Letter "e" as a bold ring with a gap.
d.ellipse([58, 78, 198, 198], outline=(255, 255, 255, 255), width=26)
d.rectangle([126, 150, 200, 178], fill=(76, 141, 255, 255))
d.rectangle([126, 150, 200, 178], fill=(76, 141, 255, 255))

# Download arrow beneath.
d.line([(128, 60), (128, 116)], fill=(255, 255, 255, 255), width=22)
d.polygon([(96, 108), (160, 108), (128, 148)], fill=(255, 255, 255, 255))

out = pathlib.Path(__file__).resolve().parent.parent / "apps" / "skiff-gui" / "src-tauri" / "icons"
out.mkdir(parents=True, exist_ok=True)
img.save(out / "icon.ico", sizes=[(16, 16), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])
img.save(out / "icon.png")
print("icons written to", out)
