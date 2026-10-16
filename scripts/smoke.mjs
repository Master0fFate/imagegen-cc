import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from "node:fs";
import http from "node:http";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const args = process.argv.slice(2);
const mcpbArg = args.indexOf("--mcpb");
const binArg = args.indexOf("--bin");
const tmp = mkdtempSync(join(tmpdir(), "igc-smoke-"));
const results = [];
const ok = (name, cond, info = "") => {
  results.push({ name, pass: !!cond });
  console.log(`${cond ? "PASS" : "FAIL"}  ${name}${info ? "  " + info : ""}`);
};

function launchSpec() {
  if (mcpbArg >= 0) {
    const bundle = resolve(args[mcpbArg + 1]);
    const dir = join(tmp, "bundle");
    mkdirSync(dir, { recursive: true });
    const pyUnzip = "import sys,zipfile,os\nz=zipfile.ZipFile(sys.argv[1])\nfor i in z.infolist():\n p=z.extract(i,sys.argv[2])\n m=(i.external_attr>>16)&0o777\n if m: os.chmod(p,m)";
    const hasUnzip = process.platform === "linux" && spawnSync("unzip", ["-v"]).status === 0;
    const x = process.platform === "linux" ? (hasUnzip ? spawnSync("unzip", ["-q", bundle, "-d", dir], { stdio: "inherit" }) : spawnSync("python3", ["-c", pyUnzip, bundle, dir], { stdio: "inherit" })) : spawnSync(process.platform === "win32" ? join(process.env.SystemRoot ?? "C:\\Windows", "System32", "tar.exe") : "tar", ["-xf", bundle, "-C", dir], { stdio: "inherit" });
    ok("bundle extracts", x.status === 0);
    const m = JSON.parse(readFileSync(join(dir, "manifest.json"), "utf8"));
    const base = m.server.mcp_config;
    const o = (base.platform_overrides ?? {})[process.platform] ?? {};
    const sub = (s) => s.replaceAll("${__dirname}", dir).replaceAll("${CLAUDE_PLUGIN_DATA}", join(tmp, "data"));
    let command = sub(o.command ?? base.command);
    if (process.platform === "win32" && !command.endsWith(".exe") && !existsSync(command)) command += ".exe";
    const env = Object.fromEntries(Object.entries({ ...(base.env ?? {}), ...(o.env ?? {}) }).map(([k, v]) => [k, sub(v)]));
    ok("manifest lists this platform", m.compatibility.platforms.includes(process.platform), m.compatibility.platforms.join(","));
    return { command, args: (o.args ?? base.args ?? []).map(sub), env };
  }
  const exe = binArg >= 0 ? resolve(args[binArg + 1]) : join(root, "target/release", process.platform === "win32" ? "imagegen-cc.exe" : "imagegen-cc");
  return { command: exe, args: [], env: {} };
}

const spec = launchSpec();
const env = { ...process.env, ...spec.env, IMAGEGEN_CC_DATA: join(tmp, "data"), IMAGEGEN_CC_KEYRING_SERVICE: `imagegen-cc-smoke-${process.pid}` };
delete env.IMAGEGEN_CC_API_KEY;
const cli = (a, input) => spawnSync(spec.command, [...spec.args, ...a], { env, input, encoding: "utf8" });

const hw = cli(["hw"]);
let hwJson = null;
try {
  hwJson = JSON.parse(hw.stdout);
} catch {}
ok("cli hw", hw.status === 0 && hwJson?.hardware?.ram_mb > 0, hwJson ? `${hwJson.hardware.os}/${hwJson.hardware.arch} ram=${hwJson.hardware.ram_mb}MB gpus=${hwJson.hardware.gpus.length} engines=${hwJson.engine_order.join(">")}` : hw.stderr);
const cat = cli(["catalog"]);
ok("cli catalog", cat.status === 0 && cat.stdout.includes("qwen-image-2.1"));

const png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
const seen = [];
const srv = http.createServer((req, res) => {
  req.resume();
  req.on("end", () => {
    seen.push(req.headers["authorization"] ?? null);
    res.setHeader("content-type", "application/json");
    res.end(JSON.stringify({ data: [{ b64_json: png }] }));
  });
});
await new Promise((r) => srv.listen(0, "127.0.0.1", r));
const port = srv.address().port;

ok("cli backend set", cli(["backend", "set", "fake", "openai", `http://127.0.0.1:${port}/v1`, "m"]).status === 0);
const ks = cli(["key", "set", "fake"], "sk-smoke-secret");
ok("cli key set", ks.status === 0, ks.stdout.trim() || ks.stderr.trim());
const vault = readFileSync(join(tmp, "data", "secrets.json"), "utf8");
const encrypted = JSON.parse(vault).enc;
ok("key not stored in plain text", !encrypted || !vault.includes("sk-smoke-secret"), encrypted ? "encrypted, master key in OS keychain" : "no OS keychain: plain file");
if (process.platform !== "win32") {
  const mode = statSync(join(tmp, "data", "secrets.json")).mode & 0o777;
  ok("secrets file mode 0600", mode === 0o600, mode.toString(8));
}
const cfgPath = join(tmp, "data", "config.json");
const cfg = JSON.parse(readFileSync(cfgPath, "utf8"));
cfg.output_dir = join(tmp, "out");
writeFileSync(cfgPath, JSON.stringify(cfg));

const p = spawn(spec.command, spec.args, { env, stdio: ["pipe", "pipe", "inherit"] });
let buf = "";
const waiters = new Map();
p.stdout.on("data", (d) => {
  buf += d;
  let i;
  while ((i = buf.indexOf("\n")) >= 0) {
    const line = buf.slice(0, i);
    buf = buf.slice(i + 1);
    if (line.trim()) {
      const m = JSON.parse(line);
      waiters.get(m.id)?.(m);
    }
  }
});
let id = 0;
const send = (method, params) => new Promise((r) => { const n = ++id; waiters.set(n, r); p.stdin.write(JSON.stringify({ jsonrpc: "2.0", id: n, method, params }) + "\n"); });
const t0 = Date.now();
const init = await Promise.race([send("initialize", { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "smoke", version: "0" } }), new Promise((r) => setTimeout(() => r(null), 15000))]);
ok("mcp initialize", init?.result?.capabilities?.tools, `${Date.now() - t0} ms`);
p.stdin.write(JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" }) + "\n");
const tools = (await send("tools/list", {})).result.tools.map((t) => t.name).sort();
ok("mcp tools", ["edit_image", "generate_image", "list_backends", "open_setup"].every((t) => tools.includes(t)), tools.join(","));
const gen = (await send("tools/call", { name: "generate_image", arguments: { prompt: "smoke test", save_to: join(tmp, "copy") + "/" } })).result;
const text = gen.content.find((c) => c.type === "text")?.text ?? "";
ok("mcp generate_image", !gen.isError && gen.content.some((c) => c.type === "image"), text.slice(0, 120));
ok("api key sent decrypted", seen.includes("Bearer sk-smoke-secret"));
ok("image saved", existsSync(join(tmp, "out")) && readdirSync(join(tmp, "out")).length > 0);
ok("save_to copy", existsSync(join(tmp, "copy")) && readdirSync(join(tmp, "copy")).length === 1);
const bad = (await send("tools/call", { name: "edit_image", arguments: { images: [cfgPath], prompt: "x" } })).result;
ok("non-image edit input refused", bad.isError && bad.content[0].text.includes("not a PNG"));
p.stdin.end();
await new Promise((r) => p.on("exit", r));
ok("server exits on stdin close", true);
ok("cli key delete", cli(["key", "delete", "fake"]).status === 0 && !existsSync(join(tmp, "data", "secrets.json")));
srv.close();
ok("keychain master key removed", cli(["key", "purge"]).status === 0);
rmSync(tmp, { recursive: true, force: true });
const failed = results.filter((r) => !r.pass);
console.log(`\n${results.length - failed.length}/${results.length} passed on ${process.platform}/${process.arch}`);
process.exit(failed.length ? 1 : 0);
