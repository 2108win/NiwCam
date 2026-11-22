# Writes the inputs both drivers read (deterministic): NV12 frames, masks, image files, texts.
# Usage: python -I gen_inputs.py <outdir> <ffmpeg>   (ffmpeg is used for image files and the face frames)
import os, subprocess, sys
import numpy as np

out, ffmpeg = sys.argv[1], sys.argv[2]
os.makedirs(out, exist_ok=True)


def make_frame(w, h, seed):
    rng = np.random.default_rng(seed)
    yy, xx = np.mgrid[0:h, 0:w]
    y = 40 + 120 * (xx / w) + 50 * (yy / h) + 25 * np.sin(xx / 37.0) * np.cos(yy / 23.0)
    y = y + rng.integers(-6, 7, size=(h, w))
    y = np.clip(y, 0, 255).astype(np.uint8)
    # A full-range black block (luma 0, below 16: the blur's wrap-around case) and a white one.
    y[h // 5 : h // 5 + h // 4, w // 8 : w // 8 + w // 5] = 0
    y[h // 2 : h // 2 + h // 6, w // 2 : w // 2 + w // 6] = 255
    cw, ch = w // 2, h // 2
    cy, cx = np.mgrid[0:ch, 0:cw]
    u = 128 + 70 * np.sin(cx / 53.0 + seed) + rng.integers(-3, 4, size=(ch, cw))
    v = 128 + 70 * np.cos(cy / 41.0 - seed) + rng.integers(-3, 4, size=(ch, cw))
    uv = np.empty((ch, cw * 2), np.uint8)
    uv[:, 0::2] = np.clip(u, 0, 255)
    uv[:, 1::2] = np.clip(v, 0, 255)
    return y, uv


def write_nv12(path, y, uv):
    with open(path, "wb") as f:
        f.write(y.tobytes())
        f.write(uv.tobytes())


frames = [("frame_a_1920x1080", 1920, 1080, 1), ("frame_b_1280x720", 1280, 720, 2),
          ("frame_c_640x480", 640, 480, 3), ("frame_d_1366x768", 1366, 768, 4),
          ("bgimg_1920x1080", 1920, 1080, 11), ("bgimg_1280x720", 1280, 720, 12),
          ("bgimg_1366x768", 1366, 768, 13), ("bgimg_720x1280", 720, 1280, 14)]
for name, w, h, seed in frames:
    y, uv = make_frame(w, h, seed)
    write_nv12(os.path.join(out, name + ".nv12"), y, uv)

# Image files: portrait PNG, wide JPEG, small BMP (so the fill's crop runs both ways).
for name, w, h, seed, ext in [("img_portrait_1000x1500", 1000, 1500, 21, "png"), ("img_wide_1600x900", 1600, 900, 22, "jpg"),
                              ("img_small_332x500", 332, 500, 23, "bmp")]:
    y, uv = make_frame(w, h, seed)
    raw = os.path.join(out, name + ".nv12")
    write_nv12(raw, y, uv)
    subprocess.run([ffmpeg, "-y", "-loglevel", "error", "-f", "rawvideo", "-pix_fmt", "nv12", "-s", f"{w}x{h}", "-i", raw,
                    "-frames:v", "1", os.path.join(out, name + "." + ext)], check=True)
    os.remove(raw)
open(os.path.join(out, "not_an_image.png"), "wb").write(b"this is not a png")

# Masks (192 x 192, 0..255): an ellipse with soft edges, and another one off to the side.
K = 192
j, i = np.mgrid[0:K, 0:K]
for name, cx, cy, rx, ry in [("mask_a", 96, 100, 50, 80), ("mask_b", 60, 90, 40, 70)]:
    d = np.sqrt(((i - cx) / rx) ** 2 + ((j - cy) / ry) ** 2)
    m = np.clip((1.15 - d) / 0.3, 0, 1)
    open(os.path.join(out, name + ".u8"), "wb").write((m * 255).astype(np.uint8).tobytes())

# Texts (UTF-8): what the card says.
open(os.path.join(out, "text_short.txt"), "wb").write("Quay lại ngay".encode("utf-8"))
open(os.path.join(out, "text_long.txt"), "wb").write(
    "Tạm dừng một chút – chúng tôi sẽ quay lại ngay sau ít phút nữa, cảm ơn các bạn đã kiên nhẫn chờ đợi!".encode("utf-8"))

# Face / person frames, from a stock portrait that ships with Windows (read in place, never copied
# into the repo) and a cat as the no-face case. Skipped when the files are missing.
base = r"C:\Windows\SystemApps\MicrosoftWindows.Client.Photon_cw5n1h2txyewy\Public\wsxpacks\Account\assets\assets\images"
person = os.path.join(base, "portrait-lighting.png")
cat = os.path.join(base, "upscale-resolution.png")


def ff(args):
    subprocess.run([ffmpeg, "-y", "-loglevel", "error"] + args, check=True)


if os.path.exists(person):
    for tag, w, h, s in [("1280x720", 1280, 720, 560), ("640x360", 640, 360, 280), ("1920x1080", 1920, 1080, 840)]:
        ox, oy = (w - s) // 2, (h - s) // 2
        ff(["-i", person, "-vf", f"scale={s}:{s}:flags=bicubic,pad={w}:{h}:{ox}:{oy}:color=0x808080,format=nv12",
            "-f", "rawvideo", os.path.join(out, f"face_one_{tag}.nv12")])
        s2 = s * 3 // 4 // 2 * 2
        ff(["-i", person, "-i", person, "-filter_complex",
            f"[0:v]scale={s2}:{s2}:flags=bicubic[a];[1:v]scale={s2}:{s2}:flags=bicubic,hflip[b];[a][b]hstack,"
            f"pad={w}:{h}:({w}-{2*s2})/2:({h}-{s2})/2:color=0x808080,format=nv12",
            "-f", "rawvideo", os.path.join(out, f"face_two_{tag}.nv12")])
        if os.path.exists(cat):
            ff(["-i", cat, "-vf", f"scale={s}:{s}:flags=bicubic,pad={w}:{h}:{ox}:{oy}:color=0x808080,format=nv12",
                "-f", "rawvideo", os.path.join(out, f"face_none_{tag}.nv12")])
    # A phone-shaped portrait frame (the person fills it), for the segmenter too.
    ff(["-i", person, "-vf", "scale=720:720:flags=bicubic,pad=720:1280:0:280:color=0x808080,format=nv12",
        "-f", "rawvideo", os.path.join(out, "face_one_720x1280.nv12")])
    print("person frames written")
else:
    print("NO PERSON IMAGE: face and segmenter inputs skipped")
