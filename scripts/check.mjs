import { readdirSync, readFileSync, statSync } from "node:fs";
import { dirname, extname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const files = [];
for (const dir of ["src", "scripts", "web", "mcpb"]) {
  const walk = (d) => {
    for (const n of readdirSync(d)) {
      const p = join(d, n);
      if (statSync(p).isDirectory()) walk(p);
      else files.push(p);
    }
  };
  walk(join(root, dir));
}

function strip(text, kind) {
  let out = "";
  let i = 0;
  let q = null;
  while (i < text.length) {
    const c = text[i];
    if (q) {
      out += c;
      if (c === "\\") {
        out += text[i + 1] ?? "";
        i += 2;
        continue;
      }
      if (c === q) q = null;
      i++;
      continue;
    }
    if (kind === "rs" && c === "r" && /^r#*"/.test(text.slice(i, i + 4))) {
      const hashes = /^r(#*)"/.exec(text.slice(i))[1];
      const end = text.indexOf(`"${hashes}`, i + 2 + hashes.length);
      out += " ".repeat(end + 1 + hashes.length - i);
      i = end + 1 + hashes.length;
      continue;
    }
    if (c === '"' || c === "`" || (c === "'" && kind !== "rs")) {
      q = c;
      out += c;
      i++;
      continue;
    }
    out += c;
    i++;
  }
  return out;
}

const bad = [];
for (const f of files) {
  const ext = extname(f);
  const text = readFileSync(f, "utf8");
  const lines = text.split("\n");
  if (ext === ".rs" || ext === ".mjs" || ext === ".js") {
    const s = strip(text, ext === ".rs" ? "rs" : "js").split("\n");
    s.forEach((l, n) => {
      if (/(^|[^:/"'])\/\/(?!\/)|\/\*/.test(l.replace(/https?:\/\/\S*/g, ""))) bad.push(`${f}:${n + 1}: ${lines[n].trim()}`);
    });
  } else if (ext === ".html") {
    lines.forEach((l, n) => {
      if (l.includes("<!--")) bad.push(`${f}:${n + 1}: ${l.trim()}`);
      if (/(^|[;{}\s])\/\*/.test(l)) bad.push(`${f}:${n + 1}: ${l.trim()}`);
    });
  } else if (ext === ".sh") {
    lines.forEach((l, n) => {
      if (n > 0 && /^\s*#/.test(l)) bad.push(`${f}:${n + 1}: ${l.trim()}`);
    });
  }
}
if (bad.length) {
  console.error(`Comments are not allowed (${bad.length}):\n${bad.join("\n")}`);
  process.exit(1);
}
const version = /^version\s*=\s*"([^"]+)"/m.exec(readFileSync(join(root, "Cargo.toml"), "utf8"))[1];
const plugin = JSON.parse(readFileSync(join(root, "plugin/.claude-plugin/plugin.json"), "utf8"));
if (plugin.version !== version || !plugin.mcpServers.includes(`/v${version}/`)) {
  console.error(`plugin.json must have version ${version} and an mcpServers URL for v${version}`);
  process.exit(1);
}
console.log(`no comments in ${files.length} files`);
