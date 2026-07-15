"""Minimal framebuffer driver for the MHS-3.5" (ILI9486) SPI display.

The MHS35 kernel overlay exposes the LCD as /dev/fb1 in 16-bit RGB565.
This module renders a Pillow Image to that framebuffer. Geometry (width,
height, bits-per-pixel, stride) is read from sysfs so the same code works
regardless of the configured rotation.
"""

from __future__ import annotations

from pathlib import Path

import numpy as np
from PIL import Image


class Framebuffer:
    def __init__(self, device: str = "/dev/fb1"):
        self.device = device
        name = Path(device).name  # e.g. "fb1"
        sysfs = Path("/sys/class/graphics") / name

        vw, vh = self._read(sysfs / "virtual_size").split(",")
        self.width = int(vw)
        self.height = int(vh)
        self.bpp = int(self._read(sysfs / "bits_per_pixel"))
        # stride = bytes per row (may include padding); fall back to width*bytes
        stride = self._read(sysfs / "stride", default="")
        self.stride = int(stride) if stride else self.width * (self.bpp // 8)

        if self.bpp != 16:
            raise RuntimeError(
                f"{device}: expected 16bpp RGB565, got {self.bpp}bpp"
            )

    @staticmethod
    def _read(path: Path, default: str | None = None) -> str:
        try:
            return path.read_text().strip()
        except FileNotFoundError:
            if default is not None:
                return default
            raise

    @property
    def size(self) -> tuple[int, int]:
        return (self.width, self.height)

    def capture(self) -> Image.Image:
        """Read the current framebuffer contents as an RGB Pillow image."""
        nbytes = self.stride * self.height
        with open(self.device, "rb") as fb:
            raw = fb.read(nbytes)
        if len(raw) < nbytes:
            raise RuntimeError(
                f"{self.device}: short read {len(raw)} < {nbytes}"
            )

        row_px = self.stride // 2
        arr = np.frombuffer(raw, dtype="<u2", count=self.height * row_px)
        arr = arr.reshape((self.height, row_px))[:, : self.width]
        r = ((arr >> 11) & 0x1F) << 3
        g = ((arr >> 5) & 0x3F) << 2
        b = (arr & 0x1F) << 3
        rgb = np.stack([r, g, b], axis=-1).astype(np.uint8)
        return Image.fromarray(rgb, "RGB")

    def show(self, image: Image.Image) -> None:
        """Blit a Pillow image to the framebuffer, converting to RGB565."""
        if image.size != self.size:
            image = image.resize(self.size)
        if image.mode != "RGB":
            image = image.convert("RGB")

        arr = np.asarray(image, dtype=np.uint16)  # (h, w, 3)
        r = (arr[:, :, 0] >> 3) & 0x1F
        g = (arr[:, :, 1] >> 2) & 0x3F
        b = (arr[:, :, 2] >> 3) & 0x1F
        rgb565 = (r << 11) | (g << 5) | b  # (h, w) uint16

        row_bytes = self.width * 2
        if self.stride == row_bytes:
            buf = rgb565.astype("<u2").tobytes()
        else:
            # pad each row up to the stride
            padded = np.zeros((self.height, self.stride // 2), dtype="<u2")
            padded[:, : self.width] = rgb565
            buf = padded.tobytes()

        with open(self.device, "wb") as fb:
            fb.write(buf)
