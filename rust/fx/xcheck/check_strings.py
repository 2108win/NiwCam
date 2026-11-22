# Every string literal with non-ASCII characters (the Vietnamese the user sees) in the four C++ files
# must appear byte for byte (UTF-8, same Unicode normalisation) in the matching Rust file.
# Usage: python -I check_strings.py <repo root>      exit code 0 = all present
import io
import re
import sys
import unicodedata
from pathlib import Path

sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding="utf-8")
root = Path(sys.argv[1])
pairs = [
    ("src/engine/StillPicture.cpp", "rust/fx/src/still_picture.rs"),
    ("src/engine/FaceDetect.cpp", "rust/fx/src/face_detect.rs"),
    ("src/engine/Segmenter.cpp", "rust/fx/src/segmenter.rs"),
    ("src/engine/WindowSource.cpp", "rust/fx/src/window_source.rs"),
]
literal = re.compile(r'L?"((?:[^"\\\n]|\\.)*)"')
bad = 0
count = 0
for cpp, rs in pairs:
    cpp_text = (root / cpp).read_text(encoding="utf-8")
    rs_bytes = (root / rs).read_bytes()
    for line_no, line in enumerate(cpp_text.splitlines(), 1):
        code = line.split("//")[0] if line.lstrip().startswith("//") else line
        for m in literal.finditer(code):
            s = m.group(1)
            if s.isascii():
                continue
            count += 1
            nfc = unicodedata.is_normalized("NFC", s)
            ok = s.encode("utf-8") in rs_bytes
            print(f"{'ok ' if ok else 'MISSING'} {cpp}:{line_no} NFC={nfc}: {s}")
            if not ok:
                bad += 1
print(f"{count} non-ASCII literals checked, {bad} missing")
sys.exit(1 if bad else 0)
