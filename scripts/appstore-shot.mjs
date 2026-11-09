// Composes App Store screenshots, 1206×2622 (iPhone 6.3"): a caption on top, the iPhone
// screenshot in a frame below, and optionally one region of it enlarged as a floating card.
// Renders HTML with headless Chrome (raw shots: xcrun devicectl device capture screenshot; a
// streaming state needs scripts/fake-pc.sh --usb --name "Máy tính Windows"). Usage:
//   node scripts/appstore-shot.mjs <out.png> <screenshot.png> "<title>" "<subtitle>" [x,y,w,h]
import { execFileSync } from "node:child_process";
import { readFileSync, writeFileSync, mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const [out, shot, title, sub, zoom] = process.argv.slice(2);
const W = 1206, H = 2622, SHOT_W = 1170;
const img = `data:image/png;base64,${readFileSync(shot).toString("base64")}`;
const esc = (s) => s.replace(/&/g, "&amp;").replace(/</g, "&lt;");

let card = "", phoneTop = 600;
if (zoom) {
  const [x, y, w, h] = zoom.split(",").map(Number);
  const cw = 1080, k = cw / w;                       // card width on the canvas, scale
  card = `<div class="card" style="width:${cw}px;height:${Math.round(h * k)}px">
    <img src="${img}" style="width:${Math.round(SHOT_W * k)}px;left:${-Math.round(x * k)}px;top:${-Math.round(y * k)}px"></div>`;
  phoneTop = 980;
}
const html = `<!doctype html><html><head><meta charset="utf-8"><style>
html,body{margin:0;width:${W}px;height:${H}px;overflow:hidden}
body{background:radial-gradient(120% 60% at 50% 0%,#12305f 0%,#0a1428 45%,#05070d 100%);
 font-family:-apple-system,"SF Pro Display","Helvetica Neue",sans-serif;color:#fff;position:relative}
.cap{position:absolute;top:150px;left:80px;right:80px;text-align:center}
h1{margin:0;font-size:84px;line-height:1.12;font-weight:800;letter-spacing:-1px}
p{margin:28px 0 0;font-size:44px;line-height:1.3;color:#a9b8d6;font-weight:500}
.phone{position:absolute;left:50%;top:${phoneTop}px;transform:translateX(-50%);width:930px;
 border-radius:96px;padding:18px;background:#1b1f2a;box-shadow:0 40px 120px rgba(0,0,0,.6),0 0 0 3px #2c3446}
.phone img{display:block;width:100%;border-radius:80px}
.card{position:absolute;left:50%;top:600px;transform:translateX(-50%);overflow:hidden;z-index:2;
 border-radius:56px;box-shadow:0 30px 90px rgba(0,0,0,.75),0 0 0 3px #3a4a6a}
.card img{position:absolute;display:block}
</style></head><body>
<div class="cap"><h1>${esc(title)}</h1>${sub ? `<p>${esc(sub)}</p>` : ""}</div>
${card}
<div class="phone"><img src="${img}"></div>
</body></html>`;
const dir = mkdtempSync(join(tmpdir(), "compose-"));
const file = join(dir, "page.html");
writeFileSync(file, html);
execFileSync("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome", [
  "--headless=new", "--disable-gpu", "--hide-scrollbars", "--force-device-scale-factor=1",
  `--window-size=${W},${H}`, `--screenshot=${resolve(out)}`, `file://${file}`,
], { stdio: "ignore" });
console.log("wrote", out);
