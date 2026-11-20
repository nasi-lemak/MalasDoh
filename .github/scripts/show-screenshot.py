"""Print a screenshot into the job log: how many colours it has (one colour
means nothing was drawn), and a small JPEG as base64 between markers, so it
can be looked at without uploading an artifact."""

import base64
import io
import sys

from PIL import Image

im = Image.open(sys.argv[1]).convert("RGB")
colours = im.getcolors(1 << 24) or []
print(f"screenshot {im.size[0]}x{im.size[1]}, {len(colours)} colours")
im.thumbnail((640, 640))
buf = io.BytesIO()
im.save(buf, "JPEG", quality=70)
data = base64.b64encode(buf.getvalue()).decode()
print("SCREENSHOT-BEGIN")
for i in range(0, len(data), 1000):
    print(data[i : i + 1000])
print("SCREENSHOT-END")
