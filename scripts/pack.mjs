import { cpSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { crc32, deflateRawSync } from "node:zlib";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const dist = join(root, "dist");
const out = join(dist, "imagegen-cc");
const version = /^version\s*=\s*"([^"]+)"/m.exec(readFileSync(join(root, "Cargo.toml"), "utf8"))[1];

const candidates = {
  win32: [
    ["target/x86_64-pc-windows-msvc/release/imagegen-cc.exe", "server/imagegen-cc.exe"],
    ["target/release/imagegen-cc.exe", "server/imagegen-cc.exe"],
  ],
  darwin: [
    ["target/universal-apple-darwin/release/imagegen-cc", "server/imagegen-cc"],
  ],
  linux: [
    ["target/x86_64-unknown-linux-musl/release/imagegen-cc", "server/imagegen-cc-x86_64"],
    ["target/aarch64-unknown-linux-musl/release/imagegen-cc", "server/imagegen-cc-aarch64"],
  ],
};

const files = new Map();
const platforms = [];
for (const [os, list] of Object.entries(candidates)) {
  const found = new Map();
  for (const [src, dst] of list) {
    const p = join(root, src);
    if (existsSync(p) && !found.has(dst)) found.set(dst, readFileSync(p));
  }
  if (found.size === 0) continue;
  platforms.push(os);
  for (const [dst, data] of found) files.set(dst, { data, exec: true });
  if (os === "linux") files.set("server/launch.sh", { data: readFileSync(join(root, "mcpb/launch.sh")), exec: true });
}
if (platforms.length === 0) {
  console.error("No release binary found. Run: cargo build --release");
  process.exit(1);
}

const overrides = {};
if (platforms.includes("win32")) overrides.win32 = { command: "${__dirname}/server/imagegen-cc.exe" };
if (platforms.includes("linux")) overrides.linux = { command: "/bin/sh", args: ["${__dirname}/server/launch.sh"] };

const manifest = {
  manifest_version: "0.3",
  name: "imagegen-cc",
  version,
  description: "Image generation and editing for Claude Code",
  author: { name: "imagegen-cc" },
  server: {
    type: "binary",
    entry_point: "server/imagegen-cc",
    mcp_config: {
      command: "${__dirname}/server/imagegen-cc",
      args: [],
      env: { IMAGEGEN_CC_DATA: "${CLAUDE_PLUGIN_DATA}" },
      platform_overrides: overrides,
    },
  },
  compatibility: { platforms },
};
files.set("manifest.json", { data: Buffer.from(JSON.stringify(manifest, null, 2)), exec: false });

function zip(entries) {
  const local = [];
  const central = [];
  let offset = 0;
  for (const [name, { data, exec }] of entries) {
    const nameBuf = Buffer.from(name);
    const packed = deflateRawSync(data, { level: 9 });
    const crc = crc32(data);
    const lh = Buffer.alloc(30);
    lh.writeUInt32LE(0x04034b50, 0);
    lh.writeUInt16LE(20, 4);
    lh.writeUInt16LE(0x0800, 6);
    lh.writeUInt16LE(8, 8);
    lh.writeUInt32LE(0x00210000, 10);
    lh.writeUInt32LE(crc, 14);
    lh.writeUInt32LE(packed.length, 18);
    lh.writeUInt32LE(data.length, 22);
    lh.writeUInt16LE(nameBuf.length, 26);
    local.push(lh, nameBuf, packed);
    const ch = Buffer.alloc(46);
    ch.writeUInt32LE(0x02014b50, 0);
    ch.writeUInt16LE(0x0314, 4);
    ch.writeUInt16LE(20, 6);
    ch.writeUInt16LE(0x0800, 8);
    ch.writeUInt16LE(8, 10);
    ch.writeUInt32LE(0x00210000, 12);
    ch.writeUInt32LE(crc, 16);
    ch.writeUInt32LE(packed.length, 20);
    ch.writeUInt32LE(data.length, 24);
    ch.writeUInt16LE(nameBuf.length, 28);
    ch.writeUInt32LE(((exec ? 0o100755 : 0o100644) << 16) >>> 0, 38);
    ch.writeUInt32LE(offset, 42);
    central.push(ch, nameBuf);
    offset += lh.length + nameBuf.length + packed.length;
  }
  const cd = Buffer.concat(central);
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0);
  end.writeUInt16LE(entries.size, 8);
  end.writeUInt16LE(entries.size, 10);
  end.writeUInt32LE(cd.length, 12);
  end.writeUInt32LE(offset, 16);
  return Buffer.concat([...local, cd, end]);
}

rmSync(dist, { recursive: true, force: true });
cpSync(join(root, "plugin"), out, { recursive: true });
const pj = join(out, ".claude-plugin/plugin.json");
const remote = process.env.MCPB_URL;
writeFileSync(pj, JSON.stringify({ ...JSON.parse(readFileSync(pj, "utf8")), version, mcpServers: remote || "./servers/imagegen-cc.mcpb" }, null, 2) + "\n");
const bundle = zip(files);
mkdirSync(join(dist, "release"), { recursive: true });
writeFileSync(join(dist, "release/imagegen-cc.mcpb"), bundle);
if (!remote) {
  mkdirSync(join(out, "servers"), { recursive: true });
  writeFileSync(join(out, "servers/imagegen-cc.mcpb"), bundle);
}
mkdirSync(join(dist, ".claude-plugin"), { recursive: true });
writeFileSync(
  join(dist, ".claude-plugin/marketplace.json"),
  JSON.stringify({ name: "imagegen-cc-local", owner: { name: "imagegen-cc" }, plugins: [{ name: "imagegen-cc", source: "./imagegen-cc", description: manifest.description }] }, null, 2) + "\n",
);
console.log(`imagegen-cc ${version}: ${platforms.join(", ")} -> ${join(out, "servers/imagegen-cc.mcpb")} (${(bundle.length / 1048576).toFixed(2)} MB)`);
