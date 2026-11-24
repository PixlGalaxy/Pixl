#!/usr/bin/env node
// Regenerate the button icons in assets/emojis/ from lucide (run by hand; the PNGs are committed).
//
// The bot uploads these PNGs as Discord application emojis at startup (src/discord/emojis.rs), so
// the list below must match `EMOJIS` there. Each file is `lynx_<name>_<color>.png`, 128x128 with a
// transparent background: icons for grey buttons are colored, icons for colored buttons are white.
//
// Needs:
//   - lucide-react, react and react-dom (the panel's `web/node_modules`; `npm ci` in web/ first)
//   - playwright-core, anywhere resolvable (for example `npm i --no-save playwright-core` in web/,
//     or set PLAYWRIGHT_DIR to a folder whose node_modules has it)
//   - an installed Chrome or Chromium (set CHROME_PATH if it is not in a usual place)
//
// Usage: node scripts/gen-emojis.mjs [--modules <dir with node_modules>]

import { existsSync, mkdirSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const OUT = join(ROOT, "assets", "emojis");
const SIZE = 128;
/// Icon size inside the 128 px canvas; the rest is padding so the glyph reads well at 16-20 px.
const GLYPH = 116;
const STROKE = 2.25;

const COLORS = {
  sky: "#38bdf8",
  red: "#f87171",
  emerald: "#34d399",
  amber: "#fbbf24",
  violet: "#a78bfa",
  slate: "#cbd5e1",
  white: "#ffffff",
};

// [emoji name, lucide icon, color]. Keep in sync with `EMOJIS` in src/discord/emojis.rs.
const EMOJIS = [
  ["pause", "pause", "sky"],
  ["play", "play", "sky"],
  ["stop", "square", "red"],
  ["skip", "skip-forward", "emerald"],
  ["queue", "list-music", "amber"],
  ["jam", "user-plus", "violet"],
  ["prev", "chevron-left", "slate"],
  ["next", "chevron-right", "slate"],
  ["replay", "rotate-ccw", "sky"],
  ["radio", "radio", "red"],
  ["radio", "radio", "white"],
  ["like", "thumbs-up", "white"],
  ["dislike", "thumbs-down", "white"],
  ["panel", "panel-top", "white"],
  ["cancel", "x", "white"],
  ["select", "check", "white"],
];

function arg(name) {
  const i = process.argv.indexOf(name);
  return i > 0 ? process.argv[i + 1] : undefined;
}

const modulesDir = resolve(arg("--modules") ?? join(ROOT, "web"));
const req = createRequire(join(modulesDir, "package.json"));

function requireFrom(name, ...bases) {
  for (const base of bases.filter(Boolean)) {
    try {
      return createRequire(join(resolve(base), "package.json"))(name);
    } catch {
      // try the next base
    }
  }
  throw new Error(`cannot find ${name}; see the header of scripts/gen-emojis.mjs`);
}

function findChrome() {
  const candidates = [
    process.env.CHROME_PATH,
    "C:/Program Files/Google/Chrome/Application/chrome.exe",
    "C:/Program Files (x86)/Google/Chrome/Application/chrome.exe",
    "/usr/bin/google-chrome",
    "/usr/bin/chromium",
    "/usr/bin/chromium-browser",
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  ];
  const found = candidates.find((p) => p && existsSync(p));
  if (!found) throw new Error("no Chrome found; set CHROME_PATH");
  return found;
}

const React = req("react");
const { renderToStaticMarkup } = req("react-dom/server");
const lucideRoot = dirname(req.resolve("lucide-react/package.json"));
const { chromium } = requireFrom("playwright-core", modulesDir, process.env.PLAYWRIGHT_DIR, process.cwd());

async function svgFor(icon, color) {
  const mod = await import(pathToFileURL(join(lucideRoot, "dist", "esm", "icons", `${icon}.mjs`)).href);
  return renderToStaticMarkup(React.createElement(mod.default, { size: GLYPH, color, strokeWidth: STROKE }));
}

mkdirSync(OUT, { recursive: true });
const browser = await chromium.launch({ executablePath: findChrome() });
const page = await browser.newPage({ viewport: { width: SIZE, height: SIZE }, deviceScaleFactor: 1 });
for (const [name, icon, color] of EMOJIS) {
  const svg = await svgFor(icon, COLORS[color]);
  await page.setContent(
    `<html><body style="margin:0;background:transparent">` +
      `<div id="c" style="width:${SIZE}px;height:${SIZE}px;display:flex;align-items:center;justify-content:center">${svg}</div>` +
      `</body></html>`,
  );
  const png = await page.locator("#c").screenshot({ omitBackground: true });
  const file = join(OUT, `lynx_${name}_${color}.png`);
  writeFileSync(file, png);
  console.log(`${file} (${png.length} bytes)`);
}
await browser.close();
