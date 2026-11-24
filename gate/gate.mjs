// Release gate for a GridCraft web build that carries the Taskforce patches.
//
// A build is published only when all of this holds, in a real browser:
//   1. it starts and posts `gridcraft:ready` to a same-origin parent (the host bridge);
//   2. it posts `gridcraft:collab hello` with the protocol and capabilities Taskforce needs;
//   3. a fixed command script replayed in the browser gives the same content hash as the
//      native engine (`gridcraft-cli run --hash`), with no command errors;
//   4. the workbook the browser writes is byte-identical to the one the native engine writes.
//
//   node gate/gate.mjs --dist <web build dir> --cli <gridcraft-cli> [--work <dir>]
//
// Chrome: $CHROME_PATH, else `google-chrome` on PATH. Exit 0 = pass, 1 = fail.
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import http from "node:http";
import path from "node:path";
import { fileURLToPath } from "node:url";
import puppeteer from "puppeteer-core";

const here = path.dirname(fileURLToPath(import.meta.url));
const arg = (name, fallback) => {
  const i = process.argv.indexOf(`--${name}`);
  return i > 0 && process.argv[i + 1] ? process.argv[i + 1] : fallback;
};
const dist = path.resolve(arg("dist", "dist/web"));
const cli = path.resolve(arg("cli", "target/release/gridcraft-cli"));
const work = path.resolve(arg("work", path.join(here, ".work")));
const script = path.join(here, "replay-script.jsonl");

const PROTOCOL = 1;
const CAPABILITIES = ["effects", "snapshot", "hash"];

const checks = [];
const check = (name, ok, detail = "") => {
  checks.push(ok);
  console.log(`${ok ? "PASS" : "FAIL"} ${name}${detail ? `  ${detail}` : ""}`);
};

fs.rmSync(work, { recursive: true, force: true });
fs.mkdirSync(work, { recursive: true });

// The native side: the same input workbook and the same script, through the CLI.
const input = path.join(work, "in.xlsx");
const nativeOut = path.join(work, "book.xlsx");
execFileSync(cli, ["run", "--sample", "budget", "--out", input], { stdio: "pipe" });
const nativeLog = execFileSync(cli, ["run", "--in", input, "--script", script, "--hash", "--out", nativeOut], { encoding: "utf8" });
const nativeHash = nativeLog.split("\n").map((l) => l.trim()).find((l) => /^[0-9a-f]{16}$/.test(l));
check("the native engine ran the script and gave a hash", !!nativeHash, nativeHash ?? nativeLog.slice(-200));
if (!nativeHash) process.exit(1);

const commands = fs
  .readFileSync(script, "utf8")
  .split("\n")
  .map((l) => l.trim())
  .filter((l) => l && !l.startsWith("#") && !l.startsWith("//"))
  .map((l) => {
    if (l.startsWith("{")) {
      const o = JSON.parse(l);
      return [o.command, o.params ?? {}];
    }
    const i = l.indexOf("=");
    return [l.slice(0, i), JSON.parse(l.slice(i + 1) || "{}")];
  });

const MIME = { ".html": "text/html; charset=utf-8", ".js": "text/javascript; charset=utf-8", ".wasm": "application/wasm", ".json": "application/json", ".css": "text/css; charset=utf-8", ".ico": "image/x-icon" };
const parent = `<!doctype html><meta charset="utf-8"><body style="margin:0"><script>
const f = document.createElement("iframe");
f.src = "./index.html?host=parent&webgl&collabtest=1";
f.style.cssText = "width:1400px;height:900px;border:0";
document.body.appendChild(f);
window.out = { ready: false, failed: null, hello: null, exec: null, save: null };
const send = (kind, body) => f.contentWindow.postMessage({ type: "gridcraft:collab", kind, body: JSON.stringify(body) }, location.origin);
window.addEventListener("message", async (e) => {
  if (e.origin !== location.origin) return;
  const d = e.data || {};
  if (d.type === "gridcraft:failed") out.failed = String(d.error || "failed");
  if (d.type === "gridcraft:ready") {
    out.ready = true;
    const b = await (await fetch("/__gate/in.xlsx")).arrayBuffer();
    f.contentWindow.postMessage({ type: "gridcraft:open", name: "book.xlsx", bytes: b }, location.origin, [b]);
  }
  if (d.type === "gridcraft:collab") {
    const body = JSON.parse(d.body);
    if (d.kind === "hello") out.hello = body;
    if (d.kind === "exec") out.exec = body;
    if (d.kind === "started") send("snapshot", { id: "gate" });
    if (d.kind === "snapshot") out.save = { ...body, bytes: d.bytes ? Array.from(new Uint8Array(d.bytes)) : null };
  }
});
window.exec = (commands) => send("exec", { id: "gate", commands });
window.snapshot = () => send("start", {});
</script></body>`;

const server = http.createServer((req, res) => {
  const url = new URL(req.url ?? "/", "http://127.0.0.1");
  if (url.pathname === "/__gate/") return void res.writeHead(200, { "content-type": MIME[".html"] }).end(parent);
  if (url.pathname === "/__gate/in.xlsx") return void res.writeHead(200, { "content-type": "application/octet-stream" }).end(fs.readFileSync(input));
  // The parent page sits in /__gate/ and frames ./index.html, so the build is served there too.
  const rel = decodeURIComponent(url.pathname.replace(/^\/__gate\//, "/"));
  const file = path.resolve(dist, `.${rel}`);
  if (file !== dist && !file.startsWith(dist + path.sep)) return void res.writeHead(403).end();
  fs.readFile(file, (err, data) => {
    if (err) return void res.writeHead(404).end();
    res.writeHead(200, { "content-type": MIME[path.extname(file).toLowerCase()] ?? "application/octet-stream" }).end(data);
  });
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const port = server.address().port;

const chrome = process.env.CHROME_PATH || execFileSync("sh", ["-c", "command -v google-chrome || command -v chromium || command -v chromium-browser"], { encoding: "utf8" }).trim();
const browser = await puppeteer.launch({
  executablePath: chrome,
  headless: true,
  defaultViewport: { width: 1400, height: 900 },
  args: ["--no-sandbox", "--disable-dev-shm-usage", "--use-angle=swiftshader", "--enable-unsafe-swiftshader", "--ignore-gpu-blocklist"],
});
const wait = async (cond, ms) => {
  const t = Date.now();
  while (Date.now() - t < ms) {
    if (await cond()) return true;
    await new Promise((r) => setTimeout(r, 200));
  }
  return false;
};

try {
  const tab = await browser.newPage();
  const pageErrors = [];
  tab.on("pageerror", (e) => pageErrors.push(e.message));
  await tab.goto(`http://127.0.0.1:${port}/__gate/`);

  const started = await wait(() => tab.evaluate(() => out.ready || out.failed !== null), 120_000);
  const failed = await tab.evaluate(() => out.failed);
  check("the build starts and posts gridcraft:ready", started && failed === null, failed ?? (started ? "" : `no message in 120 s ${pageErrors[0] ?? ""}`));
  if (!started || failed !== null) throw new Error("no bridge");

  const gotHello = await wait(() => tab.evaluate(() => out.hello !== null), 30_000);
  const hello = await tab.evaluate(() => out.hello);
  check("it posts gridcraft:collab hello", gotHello, JSON.stringify(hello));
  check(`collab protocol is ${PROTOCOL}`, hello?.protocol === PROTOCOL, String(hello?.protocol));
  check("capabilities include effects, snapshot and hash", CAPABILITIES.every((c) => hello?.capabilities?.includes(c)), JSON.stringify(hello?.capabilities));
  if (!gotHello) throw new Error("no collab hello");

  // The opened workbook is read on the next frame.
  await new Promise((r) => setTimeout(r, 2500));
  await tab.evaluate((c) => exec(c), commands);
  const ran = await wait(() => tab.evaluate(() => out.exec !== null), 60_000);
  const exec = ran ? await tab.evaluate(() => out.exec) : null;
  const errors = (exec?.results ?? []).map((r, i) => (r.error ? `${i + 1} ${commands[i]?.[0]}: ${r.error}` : null)).filter(Boolean);
  check(`the browser ran all ${commands.length} commands without an error`, ran && exec.results.length === commands.length && errors.length === 0, errors.join("; "));
  check("browser and native content hash are equal", exec?.hash === nativeHash, `browser ${exec?.hash} native ${nativeHash}`);

  await tab.evaluate(() => snapshot());
  const snapped = await wait(() => tab.evaluate(() => out.save !== null), 30_000);
  const snap = snapped ? await tab.evaluate(() => out.save) : null;
  check("the browser hands over a snapshot", !!snap?.ok && Array.isArray(snap?.bytes), snap ? String(snap.reason ?? "") : "none in 30 s");
  if (snap?.bytes) {
    const bytes = Buffer.from(snap.bytes);
    fs.writeFileSync(path.join(work, "browser.xlsx"), bytes);
    const native = fs.readFileSync(nativeOut);
    check("the browser's workbook is byte-identical to the native one", Buffer.compare(bytes, native) === 0, `${bytes.length} vs ${native.length} bytes`);
  }
} catch (e) {
  check("gate ran to the end", false, e instanceof Error ? e.message : String(e));
} finally {
  await browser.close();
  server.close();
}

const passed = checks.filter(Boolean).length;
console.log(`${passed}/${checks.length} checks passed`);
process.exit(passed === checks.length ? 0 : 1);
