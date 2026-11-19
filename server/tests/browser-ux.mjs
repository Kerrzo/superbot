// Run with Node.js 22+ and Chromium, after cargo build.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";

async function waitFor(read, label) {
  for (let i = 0; i < 150; i++) {
    const value = await read();
    if (value) return value;
    await sleep(40);
  }
  throw new Error("Timed out waiting for " + label);
}

const temp = await mkdtemp(join(tmpdir(), "superbot-browser-"));
const listener = createServer();
const port = await new Promise((resolve) => listener.listen(0, "127.0.0.1", () => resolve(listener.address().port)));
await new Promise((resolve) => listener.close(resolve));
const url = "http://127.0.0.1:" + port;
const password = "fixture-browser";
const authorization = "Basic " + Buffer.from("operator:" + password).toString("base64");
const daemon = spawn(process.env.BOTD_BIN || "target/debug/botd", [], {
  env: {
    PATH: process.env.PATH, HOST: "127.0.0.1", PORT: String(port), AUTH_PASSWORD: password,
    SCREENS: "off", DATA_DIR: join(temp, "data"), WORKSPACE_DIR: join(temp, "workspace"),
  }, stdio: ["ignore", "ignore", "inherit"],
});
const profile = join(temp, "chrome");
const browser = spawn(process.env.CHROME_BIN || "chromium", [
  "--headless=new", "--no-first-run", "--disable-extensions",
  "--remote-debugging-port=0", "--user-data-dir=" + profile, "about:blank",
], { stdio: "ignore" });
let ws;
try {
  const debugPort = await waitFor(async () => {
    try { return (await readFile(join(profile, "DevToolsActivePort"), "utf8")).split("\n")[0]; }
    catch (err) { if (err.code !== "ENOENT") throw err; }
  }, "browser");
  await waitFor(async () => (await fetch(url + "/healthz").catch(() => null))?.ok, "botd");
  const targets = await (await fetch("http://127.0.0.1:" + debugPort + "/json/list")).json();
  ws = new WebSocket(targets.find((target) => target.type === "page").webSocketDebuggerUrl);
  await new Promise((resolve, reject) => { ws.addEventListener("open", resolve); ws.addEventListener("error", reject); });

  let serial = 0;
  const pending = new Map();
  ws.addEventListener("message", (event) => {
    const reply = JSON.parse(event.data);
    if (pending.has(reply.id)) {
      pending.get(reply.id)(reply);
      pending.delete(reply.id);
    }
  });
  async function command(method, params = {}) {
    const id = ++serial;
    const result = new Promise((resolve) => pending.set(id, resolve));
    ws.send(JSON.stringify({ id, method, params }));
    const reply = await result;
    assert.ok(!reply.error, JSON.stringify(reply.error));
    return reply.result;
  }
  async function evaluate(expression) {
    const result = await command("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true });
    assert.ok(!result.exceptionDetails, result.exceptionDetails?.exception?.description);
    return result.result?.value;
  }
  const q = (selector) => "document.querySelector(" + JSON.stringify(selector) + ")";
  const click = (selector) => evaluate(q(selector) + ".click()");
  const visible = (selector) => waitFor(() => evaluate(q(selector) + "?.getClientRects().length"), selector);
  await command("Network.enable");
  await command("Network.setExtraHTTPHeaders", { headers: { Authorization: authorization } });
  await command("Page.navigate", { url });
  await visible("#suggestions button");
  await click("#suggestions button:first-child");
  assert.equal(await evaluate(q("#input") + ".value"), "Set up a routine to ");
  await click("#suggestions button:last-child");
  assert.equal(await evaluate(q("#input") + ".value"), "Your job: ");
  const state = await (await fetch(url + "/api/state", { headers: { Authorization: authorization } })).json();
  const id = state.bots.find((bot) => bot.primary).id;
  const history = await (await fetch(url + "/api/bots/" + id + "/messages", { headers: { Authorization: authorization } })).json();
  assert.deepEqual(history, []);

  await click("#bot-pill");
  await visible('[data-tab="activity"]');
  assert.equal(await evaluate(q("#details") + '.textContent.includes("Suggested")'), false);
  await click('[data-tab="settings"]');
  await click("#d-pause");
  await waitFor(() => evaluate(q("#input") + ".disabled"), "paused composer");
  assert.equal(await evaluate(q("#suggestions") + ".hidden"), true);
  await click("#d-pause");
  await visible("#suggestions button");

  await click("#mcp-open");
  await waitFor(() => evaluate("!" + q("#mcp-add") + ".disabled"), "MCP catalogue");
  await click("#mcp-add");
  await evaluate(q("#mcp-name") + '.value="Fixture service";' + q("#mcp-transport") +
    '.value="stdio";' + q("#mcp-transport") + '.dispatchEvent(new Event("change"));' +
    q("#mcp-command") + '.value="/missing-browser-fixture"');
  await click("#mcp-save");
  await visible('[data-mcp-action="test"]');
  await click('[data-mcp-action="test"]');
  await waitFor(() => evaluate(q("#mcp-list") + '.textContent.includes("Could not start")'), "connection error status");
  await click("#mcp-close");
  await visible("[data-mcp-grant]");
  assert.equal(await evaluate(q("[data-mcp-grant]") + '.getAttribute("aria-checked")'), "false");
  await click("[data-mcp-grant]");
  await waitFor(() => evaluate(q("[data-mcp-grant]") + '.getAttribute("aria-checked") === "true"'), "MCP grant");
  await click("[data-mcp-grant]");
  await waitFor(() => evaluate(q("[data-mcp-grant]") + '.getAttribute("aria-checked") === "false"'), "MCP revocation");
  console.log("Browser passed: suggestion prefill, pause and resume, MCP service form, connection status and per-bot toggle.");
} finally {
  ws?.close();
  for (const child of [browser, daemon]) {
    if (child.exitCode === null && child.signalCode === null) {
      child.kill();
      await Promise.race([new Promise((resolve) => child.once("exit", resolve)), sleep(1200)]);
    }
  }
  await rm(temp, { recursive: true, force: true, maxRetries: 3 });
}
