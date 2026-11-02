// Hosts smelt's docs player and assets unchanged, then screenshots its replay frames.
import path from "node:path";

type Replay = {
  cols: number;
  rows: number;
  frames: { ms: number; l: (string | null)[] }[];
};

async function assets(smeltRepo: string) {
  const site = path.join(smeltRepo, "docs/docs");
  const shots = path.join(smeltRepo, "docs/shots");
  const fonts = path.join(shots, "node_modules/jetbrains-mono/fonts/webfonts");
  const files = new Map([
    ["/stylesheets/extra.css", path.join(site, "stylesheets/extra.css")],
    ["/javascripts/replay.js", path.join(site, "javascripts/replay.js")],
    ["/assets/wallpaper.jpg", path.join(site, "assets/wallpaper.jpg")],
    [
      "/assets/fonts/SymbolsNerdFontMono-subset.woff2",
      path.join(site, "assets/fonts/SymbolsNerdFontMono-subset.woff2"),
    ],
    [
      "/__fonts/JetBrainsMono-Regular.woff2",
      path.join(fonts, "JetBrainsMono-Regular.woff2"),
    ],
    [
      "/__fonts/JetBrainsMono-Bold.woff2",
      path.join(fonts, "JetBrainsMono-Bold.woff2"),
    ],
  ]);
  const playwright = path.join(shots, "node_modules/playwright-core/index.mjs");
  for (const file of [...files.values(), playwright]) {
    if (!(await Bun.file(file).exists())) {
      throw new Error(`Missing ${file}. See scripts/README.md for setup.`);
    }
  }
  const { chromium } = await import(playwright);
  return { files, chromium };
}

async function render(
  smeltRepo: string,
  replay: Replay,
  outputDir: string,
  mode?: string,
) {
  const { files, chromium } = await assets(smeltRepo);
  const html = `<!doctype html><meta charset="utf-8">
<link rel="stylesheet" href="/stylesheets/extra.css">
<style>
  @font-face{font-family:"JetBrains Mono";font-weight:400;src:url(/__fonts/JetBrainsMono-Regular.woff2)}
  @font-face{font-family:"JetBrains Mono";font-weight:700;src:url(/__fonts/JetBrainsMono-Bold.woff2)}
  :root{--md-code-font-family:"JetBrains Mono"}
  body{margin:0}
  .term__keys{display:none}
  .md-typeset figure.term{width:${replay.cols * 9}px}
</style>
<div class="md-typeset"><figure class="term" data-replay="/replay.json"><pre class="term__screen"></pre></figure></div>
<script src="/javascripts/replay.js"></script>`;
  const server = Bun.serve({
    hostname: "127.0.0.1",
    port: 0,
    fetch(req) {
      const url = new URL(req.url).pathname;
      if (url === "/")
        return new Response(html, { headers: { "content-type": "text/html" } });
      if (url === "/replay.json") return Response.json(replay);
      const file = files.get(url);
      return file
        ? new Response(Bun.file(file))
        : new Response("Not found", { status: 404 });
    },
  });
  let browser;
  try {
    browser = await chromium.launch({
      executablePath: process.env.CHROME_PATH,
    });
    const page = await browser.newPage({
      viewport: { width: replay.cols * 9 + 100, height: 1000 },
      reducedMotion: "reduce",
    });
    await page.goto(server.url.href);
    const figure = page.locator("figure.term");
    await figure.evaluate(async (el) => {
      const player = el.__replay;
      await player.load();
      player.pause();
      await document.fonts.ready;
      player.fit();
      player.show(0, true);
      if (!document.fonts.check('15px "JetBrains Mono"'))
        throw new Error("JetBrains Mono did not load");
    });
    const indices =
      mode === "--stills"
        ? [
            ...new Set([
              0,
              Math.floor(replay.frames.length / 2),
              replay.frames.length - 1,
            ]),
          ]
        : replay.frames.map((_, n) => n);
    for (const n of indices) {
      await figure.evaluate((el, n) => el.__replay.show(n, true), n);
      await Bun.write(
        path.join(outputDir, `${String(n).padStart(4, "0")}.png`),
        await figure.screenshot(),
      );
    }
    console.log(`Rendered ${indices.length} frames with smelt's docs player`);
  } finally {
    await browser?.close();
    server.stop(true);
  }
}

if (import.meta.main) {
  const [smeltRepo, replayFile, outputDir, mode] = process.argv.slice(2);
  if (!smeltRepo || !replayFile)
    throw new Error(
      "Usage: bun scripts/demo-render.ts SMELT_REPO REPLAY OUTPUT_DIR [--stills]",
    );
  if (replayFile === "--check") {
    const { chromium } = await assets(path.resolve(smeltRepo));
    const browser = await chromium.launch({
      executablePath: process.env.CHROME_PATH,
    });
    await browser.close();
  } else {
    if (!outputDir) throw new Error("Missing output directory");
    const replay: Replay = await Bun.file(replayFile).json();
    await render(path.resolve(smeltRepo), replay, outputDir, mode);
  }
}
