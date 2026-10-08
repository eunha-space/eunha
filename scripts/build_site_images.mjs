// Renders the raster images site/index.html references — the social-preview
// card and the iOS touch icon — and the ones the web frontend serves for an
// instance that has uploaded none of its own (frontend/public/images/preview.png,
// the instance API's default thumbnail, and the icons/android-chrome-*.png it
// lists for `SiteUpload::ANDROID_ICON_SIZES`) from the brand symbol, so they
// never drift from the SVG they are cut from.
//
//   mise run site:images
//
// Playwright is the frontend's dev dependency rather than a new one; this
// reuses it because a headless Chromium is already the thing that agrees with
// browsers about how the SVG paints.

import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
import { dirname, resolve } from "node:path";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const site = resolve(root, "site");
const frontendPublic = resolve(root, "frontend/public");

// Node resolves a bare import from the *script's* directory, and scripts/ has
// no node_modules; frontend/ is where playwright is installed. @playwright/test
// rather than "playwright" because that is what frontend/package.json declares,
// and pnpm links only what is declared.
const { chromium } = createRequire(resolve(root, "frontend/package.json"))(
  "@playwright/test",
);

const NAVY = "#00102a";
const CREAM = "#f8f4eb";
const MINT = "#7ae2d1";
const LILAC = "#b197ed";

// The symbol's own bounding box, measured rather than assumed: the artboard is
// 800x700 but the mark occupies 589x511 of it, and centring the artboard would
// leave the mark visibly high and small.
const BBOX = { x: 105.5, y: 94.6, w: 589, h: 510.9 };

// Color_01 — the variant drawn for dark grounds, which is what both images use.
const symbol = readFileSync(resolve(site, "favicon.svg"), "utf8")
  .replace(/<!--[\s\S]*?-->/g, "")
  .replace(/<style>[\s\S]*?<\/style>/, "")
  .replace(/viewBox="[^"]*"/, `viewBox="${BBOX.x} ${BBOX.y} ${BBOX.w} ${BBOX.h}"`)
  .replace(/class="body"/g, `fill="${CREAM}"`)
  .replace(/class="accent"/g, `fill="${LILAC}"`)
  .replace(/class="ring"/g, `fill="${MINT}"`);

const font = `ui-sans-serif, system-ui, -apple-system, "Segoe UI", Roboto, sans-serif`;

const og = `<style>
  html,body { margin:0; }
  body {
    width:1200px; height:630px; background:${NAVY}; color:${CREAM};
    font-family:${font}; display:flex; align-items:center; gap:72px;
    padding:0 88px; box-sizing:border-box;
  }
  svg { width:340px; height:auto; flex:none; }
  h1 { font-size:104px; letter-spacing:-.04em; font-weight:600; margin:0 0 18px; line-height:1; }
  p  { font-size:31px; line-height:1.4; color:#a9b4c6; margin:0; max-width:15em; }
  .u { color:${MINT}; }
</style>
${symbol}
<div>
  <h1>eunha</h1>
  <p>Mastodon, reimplemented in Rust. <span class="u">100% schema compatible</span>, so it drops in on the database you already have.</p>
</div>`;

const icon = `<style>
  html,body { margin:0; }
  body {
    width:180px; height:180px; background:${NAVY};
    display:flex; align-items:center; justify-content:center;
  }
  svg { width:142px; height:auto; }
</style>
${symbol}`;

// The instance thumbnail: the symbol alone, as Mastodon's preview.png is a
// picture rather than a card, so that it reads under any instance's name.
const preview = `<style>
  html,body { margin:0; }
  body {
    width:1200px; height:630px; background:${NAVY};
    display:flex; align-items:center; justify-content:center;
  }
  svg { width:520px; height:auto; }
</style>
${symbol}`;

// `SiteUpload::ANDROID_ICON_SIZES`.
const ANDROID_ICON_SIZES = [36, 48, 72, 96, 144, 192, 256, 384, 512];
const androidIcon = (size) => `<style>
  html,body { margin:0; }
  body {
    width:${size}px; height:${size}px; background:${NAVY};
    display:flex; align-items:center; justify-content:center;
  }
  svg { width:${Math.round(size * 0.79)}px; height:auto; }
</style>
${symbol}`;

const browser = await chromium.launch();
for (const [base, name, html, width, height] of [
  [site, "og.png", og, 1200, 630],
  [site, "apple-touch-icon.png", icon, 180, 180],
  [frontendPublic, "images/preview.png", preview, 1200, 630],
  ...ANDROID_ICON_SIZES.map((size) => [
    frontendPublic,
    `icons/android-chrome-${size}x${size}.png`,
    androidIcon(size),
    size,
    size,
  ]),
]) {
  const page = await browser.newPage({ viewport: { width, height } });
  await page.setContent(html, { waitUntil: "load" });
  const out = resolve(base, name);
  mkdirSync(dirname(out), { recursive: true });
  writeFileSync(out, await page.screenshot());
  await page.close();
  console.log(`${out.slice(root.length + 1)}  ${width}x${height}`);
}
await browser.close();
