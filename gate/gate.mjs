// Release gate for a GridCraft web build that carries the Taskforce patches.
//
// A build is published only when all of this holds, in a real browser:
//   1. it starts and posts `gridcraft:ready` to a same-origin parent (the host bridge);
//   2. it says hello on the co-editing channel and on the host channel with the protocols,
//      capabilities and effect fields Taskforce relies on (contract.json: only ever added to);
//   3. a fixed command script replayed in the browser gives the same content hash as the
//      native engine (`gridcraft-cli run --hash`), with no command errors, and the workbook the
//      browser writes is byte-identical to the one the native engine writes;
//   4. meaning, not bytes: a second script's workbook, read without the engine (semantic.mjs),
//      says what semantic-golden.json says, from the native engine, from the browser, and again
//      after the engine opened and saved it;
//   5. old and new together, when `--prev-dist` names the build in use before this one: the
//      previous build takes the edits this build sends while co-editing, and this build takes
//      the previous build's (people have the old one open for a while after an update).
//
//   node gate/gate.mjs --dist <web build dir> --cli <gridcraft-cli> [--prev-dist <dir>] [--work <dir>]
//                      [--update-golden]
//
// Chrome: $CHROME_PATH, else `google-chrome` on PATH. Exit 0 = pass, 1 = fail.
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import http from "node:http";
import path from "node:path";
import { fileURLToPath } from "node:url";
import puppeteer from "puppeteer-core";
import { differences, readWorkbook } from "./semantic.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const arg = (name, fallback) => {
  const i = process.argv.indexOf(`--${name}`);
  return i > 0 && process.argv[i + 1] ? process.argv[i + 1] : fallback;
};
const dist = path.resolve(arg("dist", "dist/web"));
const prevDist = arg("prev-dist", "") ? path.resolve(arg("prev-dist", "")) : null;
const cli = path.resolve(arg("cli", "target/release/gridcraft-cli"));
const work = path.resolve(arg("work", path.join(here, ".work")));
const updateGolden = process.argv.includes("--update-golden");
const goldenPath = path.join(here, "semantic-golden.json");
const contract = JSON.parse(fs.readFileSync(path.join(here, "contract.json"), "utf8"));

const checks = [];
const check = (name, ok, detail = "") => {
  checks.push(!!ok);
  console.log(`${ok ? "PASS" : "FAIL"} ${name}${detail ? `  ${detail}` : ""}`);
};
const note = (text) => console.log(`NOTE ${text}`);

function scriptOf(file) {
  return fs
    .readFileSync(path.join(here, file), "utf8")
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
}

fs.rmSync(work, { recursive: true, force: true });
fs.mkdirSync(work, { recursive: true });

// The native side: the same input workbook and the same scripts, through the CLI.
const input = path.join(work, "in.xlsx");
execFileSync(cli, ["run", "--sample", "budget", "--out", input], { stdio: "pipe" });
function native(script, out) {
  const log = execFileSync(cli, ["run", "--in", input, "--script", path.join(here, script), "--hash", "--out", out], { encoding: "utf8" });
  return log.split("\n").map((l) => l.trim()).find((l) => /^[0-9a-f]{16}$/.test(l)) ?? null;
}
const nativeOut = path.join(work, "book.xlsx");
const nativeHash = native("replay-script.jsonl", nativeOut);
check("the native engine ran the script and gave a hash", !!nativeHash, nativeHash ?? "");
if (!nativeHash) process.exit(1);
const commands = scriptOf("replay-script.jsonl");

const MIME = { ".html": "text/html; charset=utf-8", ".js": "text/javascript; charset=utf-8", ".wasm": "application/wasm", ".json": "application/json", ".css": "text/css; charset=utf-8", ".ico": "image/x-icon" };

// The page that plays the host: frames the build, hands it the workbook, and plays the server
// for co-editing (confirms each batch to its sender and keeps it).
const parent = `<!doctype html><meta charset="utf-8"><body style="margin:0"><script>
const f = document.createElement("iframe");
f.src = "./index.html?host=parent&webgl&collabtest=1";
f.style.cssText = "width:1400px;height:900px;border:0";
document.body.appendChild(f);
window.out = { ready: false, failed: null, hello: null, tf: null, exec: null, started: null, save: null, ops: [], trouble: [] };
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
  if (d.type === "gridcraft:tf" && d.kind === "hello") out.tf = JSON.parse(d.body);
  if (d.type === "gridcraft:collab") {
    const body = JSON.parse(d.body);
    if (d.kind === "hello") out.hello = body;
    if (d.kind === "exec") out.exec = body;
    if (d.kind === "started") out.started = body;
    if (d.kind === "op") { out.ops.push(body); send("ops", [{ kind: "confirm", seq: body.seq }]); }
    if (d.kind === "refused" || d.kind === "error") out.trouble.push(d.kind + " " + String(body && body.message || ""));
    if (d.kind === "snapshot") out.save = { ...body, bytes: d.bytes ? Array.from(new Uint8Array(d.bytes)) : null };
  }
});
window.exec = (commands) => { out.exec = null; send("exec", { id: "gate", commands }); };
window.start = () => send("start", {});
window.apply = (batches) => { for (const b of batches) send("ops", [{ kind: "apply", effects: b.effects }]); };
window.snapshot = () => { out.save = null; send("snapshot", { id: "gate" }); };
</script></body>`;

/** A throwaway server for one build, with the host page and the input workbook beside it. */
async function serve(dir) {
  const server = http.createServer((req, res) => {
    const url = new URL(req.url ?? "/", "http://127.0.0.1");
    if (url.pathname === "/__gate/") return void res.writeHead(200, { "content-type": MIME[".html"] }).end(parent);
    if (url.pathname === "/__gate/in.xlsx") return void res.writeHead(200, { "content-type": "application/octet-stream" }).end(fs.readFileSync(input));
    // The parent page sits in /__gate/ and frames ./index.html, so the build is served there too.
    // An installed build asks for its wasm as `_bg.wasm.bin`; the file is the .wasm.
    const rel = decodeURIComponent(url.pathname.replace(/^\/__gate\//, "/")).replace(/_bg\.wasm\.bin$/, "_bg.wasm");
    const file = path.resolve(dir, `.${rel}`);
    if (file !== dir && !file.startsWith(dir + path.sep)) return void res.writeHead(403).end();
    fs.readFile(file, (err, data) => {
      if (err) return void res.writeHead(404).end();
      res.writeHead(200, { "content-type": MIME[path.extname(file).toLowerCase()] ?? "application/octet-stream" }).end(data);
    });
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  return server;
}

const chrome = process.env.CHROME_PATH || execFileSync("sh", ["-c", "command -v google-chrome || command -v chromium || command -v chromium-browser"], { encoding: "utf8" }).trim();
const browser = await puppeteer.launch({
  executablePath: chrome,
  headless: true,
  defaultViewport: { width: 1400, height: 900 },
  args: ["--no-sandbox", "--disable-dev-shm-usage", "--use-angle=swiftshader", "--enable-unsafe-swiftshader", "--ignore-gpu-blocklist"],
});
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const wait = async (cond, ms) => {
  const t = Date.now();
  while (Date.now() - t < ms) {
    if (await cond()) return true;
    await sleep(200);
  }
  return false;
};

/** One window of a build with the input workbook open. `close()` when done. */
async function openBuild(server, label) {
  const tab = await browser.newPage();
  const pageErrors = [];
  tab.on("pageerror", (e) => pageErrors.push(e.message));
  await tab.goto(`http://127.0.0.1:${server.address().port}/__gate/`);
  const started = await wait(() => tab.evaluate(() => out.ready || out.failed !== null), 120_000);
  const failed = await tab.evaluate(() => out.failed);
  if (!started || failed !== null) {
    await tab.close();
    throw new Error(`${label} did not start: ${failed ?? pageErrors[0] ?? "no message in 120 s"}`);
  }
  await wait(() => tab.evaluate(() => out.hello !== null), 30_000);
  // The opened workbook is read on the next frame.
  await sleep(2500);
  const w = {
    tab,
    state: () => tab.evaluate(() => ({ hello: out.hello, tf: out.tf, trouble: out.trouble, ops: out.ops })),
    async exec(list) {
      await tab.evaluate((c) => exec(c), list);
      const ran = await wait(() => tab.evaluate(() => out.exec !== null), 60_000);
      const result = ran ? await tab.evaluate(() => out.exec) : null;
      const errors = (result?.results ?? []).map((r, i) => (r.error ? `${i + 1} ${list[i]?.[0]}: ${r.error}` : null)).filter(Boolean);
      return { ok: ran && result.results.length === list.length && errors.length === 0, errors, hash: result?.hash ?? null };
    },
    async start() {
      await tab.evaluate(() => start());
      return (await wait(() => tab.evaluate(() => out.started !== null), 30_000)) ? tab.evaluate(() => out.started.hash) : null;
    },
    /** Other people's batches, as the server relays them; true when none of them was refused. */
    async apply(batches) {
      await tab.evaluate((b) => apply(b), batches);
      await sleep(1500);
      return (await tab.evaluate(() => out.trouble)).length === 0;
    },
    /** Every batch this window sent, once it has stopped sending. */
    async sent() {
      let n = -1;
      await wait(async () => {
        const now = await tab.evaluate(() => out.ops.length);
        const still = now === n;
        n = now;
        return still && now > 0;
      }, 20_000);
      return tab.evaluate(() => out.ops);
    },
    async snapshot() {
      let snap = null;
      // A snapshot waits until nothing is pending: ask again while it says so.
      for (let i = 0; i < 20 && !snap?.ok; i++) {
        await tab.evaluate(() => snapshot());
        await wait(() => tab.evaluate(() => out.save !== null), 30_000);
        snap = await tab.evaluate(() => out.save);
        if (!snap?.ok) await sleep(500);
      }
      return snap?.ok && Array.isArray(snap.bytes) ? Buffer.from(snap.bytes) : null;
    },
    close: () => tab.close(),
  };
  return w;
}

const server = await serve(dist);
const prevServer = prevDist ? await serve(prevDist) : null;
try {
  /* ── 1–3: start, hello, parity with the native engine ── */
  const a = await openBuild(server, "the build");
  check("the build starts and posts gridcraft:ready", true);
  const { hello, tf } = await a.state();
  check("it posts gridcraft:collab hello", !!hello, JSON.stringify(hello));
  check(`collab protocol is ${contract.collabProtocol}`, hello?.protocol === contract.collabProtocol, String(hello?.protocol));
  const lacksCollab = contract.collabCapabilities.filter((c) => !hello?.capabilities?.includes(c));
  check("its co-editing capabilities include every one in the contract", lacksCollab.length === 0, lacksCollab.length ? `missing ${lacksCollab.join(", ")}` : JSON.stringify(hello?.capabilities));
  check(`it posts gridcraft:tf hello with host protocol ${contract.hostProtocol}`, tf?.protocol === contract.hostProtocol, JSON.stringify(tf)?.slice(0, 200));
  // Files in use depend on these: a build that no longer announces one would open them read-only.
  const lacks = contract.capabilities.filter((c) => !tf?.capabilities?.includes(c));
  check("its host capabilities include every one in the contract", lacks.length === 0, lacks.length ? `missing ${lacks.join(", ")}` : "");
  const added = (tf?.capabilities ?? []).filter((c) => !contract.capabilities.includes(c));
  check("every capability it announces is in the contract (add new ones there, with the change that adds them)", added.length === 0, added.join(", "));
  if (!hello) throw new Error("no collab hello");

  const ran = await a.exec(commands);
  check(`the browser ran all ${commands.length} commands without an error`, ran.ok, ran.errors.join("; "));
  check("browser and native content hash are equal", ran.hash === nativeHash, `browser ${ran.hash} native ${nativeHash}`);
  await a.start();
  const bytes = await a.snapshot();
  check("the browser hands over a snapshot", !!bytes);
  if (bytes) {
    fs.writeFileSync(path.join(work, "browser.xlsx"), bytes);
    const nativeBytes = fs.readFileSync(nativeOut);
    check("the browser's workbook is byte-identical to the native one", Buffer.compare(bytes, nativeBytes) === 0, `${bytes.length} vs ${nativeBytes.length} bytes`);
  }
  await a.close();

  /* ── 4: what the file means ── */
  const semOut = path.join(work, "semantic.xlsx");
  const semHash = native("semantic-script.jsonl", semOut);
  const nativeMeaning = readWorkbook(fs.readFileSync(semOut));
  if (updateGolden) {
    fs.writeFileSync(goldenPath, `${JSON.stringify(nativeMeaning, null, 1)}\n`);
    note(`wrote ${path.relative(process.cwd(), goldenPath)}: read it before committing it`);
  }
  const golden = JSON.parse(fs.readFileSync(goldenPath, "utf8"));
  const same = (name, meaning) => {
    const d = differences(meaning, golden);
    check(name, d.length === 0, d.join(" | "));
  };
  same("the native engine's workbook says what the golden says (cells, formulas, styles, merges, validation, names, formatting inside text)", nativeMeaning);
  const resaved = path.join(work, "semantic-resaved.xlsx");
  execFileSync(cli, ["run", "--in", semOut, "--out", resaved], { stdio: "pipe" });
  same("it still says so after the engine opened and saved it again", readWorkbook(fs.readFileSync(resaved)));
  const semantic = scriptOf("semantic-script.jsonl");
  const b = await openBuild(server, "the build");
  const semRan = await b.exec(semantic);
  check(`the browser ran the ${semantic.length} commands of the semantic script`, semRan.ok && semRan.hash === semHash, semRan.errors.join("; ") || `browser ${semRan.hash} native ${semHash}`);
  await b.start();
  const semBytes = await b.snapshot();
  if (semBytes) fs.writeFileSync(path.join(work, "semantic-browser.xlsx"), semBytes);
  same("the browser's workbook says what the golden says", semBytes ? readWorkbook(semBytes) : {});
  await b.close();

  /* ── 5: the edits a build sends while co-editing, and old and new together ── */
  const effects = scriptOf("effects-script.jsonl");
  const c = await openBuild(server, "the build");
  await c.start();
  const sentOk = await c.exec(effects);
  const batches = await c.sent();
  const { trouble } = await c.state();
  check(`while co-editing, the build ran the ${effects.length} commands of the effects script and sent them as effects`, sentOk.ok && trouble.length === 0 && batches.length > 0, sentOk.errors.join("; ") || trouble.join("; ") || `${batches.length} batches`);
  const fields = new Set(batches.flatMap((x) => x.effects.flatMap((e) => Object.keys(e))));
  const unknown = [...fields].filter((k) => !contract.effectFields.includes(k));
  // A field the server's schema does not know is an edit the server refuses.
  check("its effects use only the fields in the contract (a new field goes there and into the server's schema first)", unknown.length === 0, unknown.join(", "));
  const newBytes = await c.snapshot();
  await c.close();
  const newMeaning = newBytes ? readWorkbook(newBytes) : null;
  // What a build without formatting inside text and option sets keeps of a workbook.
  const basics = (m) => ({ sheets: (m?.sheets ?? []).map((s) => ({ ...s, cells: Object.fromEntries(Object.entries(s.cells ?? {}).map(([k, v]) => [k, { ...v, rich: undefined }])) })) });

  if (!prevServer) {
    note("no --prev-dist: the old-and-new checks did not run (there is no earlier build, or it was not given)");
  } else {
    const old = await openBuild(prevServer, "the previous build");
    const oldHello = (await old.state()).hello;
    note(`previous build: ${oldHello?.build ?? "unknown"}`);
    await old.start();
    const took = await old.apply(batches);
    check("the previous build takes this build's edits without an error", took, (await old.state()).trouble.join("; "));
    const oldBytes = await old.snapshot();
    const d1 = differences(basics(oldBytes ? readWorkbook(oldBytes) : null), basics(newMeaning));
    check("and ends on the same cells, formulas and styles", d1.length === 0, d1.join(" | "));
    await old.close();

    const v1 = scriptOf("v1-script.jsonl");
    const old2 = await openBuild(prevServer, "the previous build");
    await old2.start();
    const oldRan = await old2.exec(v1);
    const oldBatches = await old2.sent();
    check(`the previous build ran the ${v1.length} commands of the protocol-1 script while co-editing`, oldRan.ok && oldBatches.length > 0, oldRan.errors.join("; "));
    const oldSaid = await old2.snapshot();
    await old2.close();
    const fresh = await openBuild(server, "the build");
    await fresh.start();
    const took2 = await fresh.apply(oldBatches);
    check("this build takes the previous build's edits without an error", took2, (await fresh.state()).trouble.join("; "));
    const freshBytes = await fresh.snapshot();
    const d2 = differences(basics(freshBytes ? readWorkbook(freshBytes) : null), basics(oldSaid ? readWorkbook(oldSaid) : null));
    check("and ends on the same cells, formulas and styles", d2.length === 0, d2.join(" | "));
    await fresh.close();
  }
} catch (e) {
  check("gate ran to the end", false, e instanceof Error ? e.message : String(e));
} finally {
  await browser.close();
  server.close();
  prevServer?.close();
}

const passed = checks.filter(Boolean).length;
console.log(`${passed}/${checks.length} checks passed`);
process.exit(passed === checks.length ? 0 : 1);
