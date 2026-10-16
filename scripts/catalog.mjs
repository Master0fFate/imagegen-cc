import { writeFileSync, mkdirSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const SD_TAG = "master-968-8cd731e";
const ROCM = { index: "https://stable.repo.amd.com/rocm/whl-next", version: "10.1.0" };

const QI = "Comfy-Org/Qwen-Image-2.1";
const QG = "leejet/Qwen-Image-2.1-GGUF";
const QV = "Qwen/Qwen3-VL-8B-Instruct-GGUF";
const ZT = "Comfy-Org/z_image_turbo";
const ZG = "leejet/Z-Image-Turbo-GGUF";
const Q4 = "unsloth/Qwen3-4B-Instruct-2507-GGUF";

const recipes = [
  {
    id: "qwen-image-2.1",
    name: "Qwen-Image 2.1",
    about: "Best quality and prompt following. Strong with people, text in images, and editing.",
    license: "Apache-2.0",
    edit: true,
    variants: [
      { id: "int8", label: "Full quality (int8)", min_vram_gb: 20, files: { diffusion: [QI, "diffusion_models/qwen_image_2.1_int8_convrot.safetensors"], llm: [QI, "text_encoders/qwen3vl_8b_int8_convrot.safetensors"], llm_vision: [QV, "mmproj-Qwen3VL-8B-Instruct-Q8_0.gguf"], vae: [QI, "vae/qwen_image_2.1_vae_bf16.safetensors"] }, args: ["--cfg-scale", "3.5", "--steps", "30", "--sampling-method", "euler"] },
      { id: "q4", label: "Balanced (Q4)", min_vram_gb: 12, files: { diffusion: [QG, "qwen_image_2.1-Q4_K.gguf"], llm: [QV, "Qwen3VL-8B-Instruct-Q4_K_M.gguf"], llm_vision: [QV, "mmproj-Qwen3VL-8B-Instruct-Q8_0.gguf"], vae: [QI, "vae/qwen_image_2.1_vae_bf16.safetensors"] }, args: ["--cfg-scale", "3.5", "--steps", "30", "--sampling-method", "euler"] },
      { id: "q3-offload", label: "Low VRAM (Q3, uses system RAM)", min_vram_gb: 8, min_ram_gb: 16, files: { diffusion: [QG, "qwen_image_2.1-Q3_K.gguf"], llm: [QV, "Qwen3VL-8B-Instruct-Q4_K_M.gguf"], llm_vision: [QV, "mmproj-Qwen3VL-8B-Instruct-Q8_0.gguf"], vae: [QI, "vae/qwen_image_2.1_vae_bf16.safetensors"] }, args: ["--cfg-scale", "3.5", "--steps", "30", "--sampling-method", "euler", "--offload-to-cpu"] },
    ],
  },
  {
    id: "qwen-image-2.1-turbo",
    name: "Qwen-Image 2.1 Turbo",
    about: "Same model family, 8 steps instead of 30. About 4x faster, slightly less detail.",
    license: "Apache-2.0",
    edit: true,
    variants: [
      { id: "int8", label: "Full quality (int8)", min_vram_gb: 20, files: { diffusion: [QI, "diffusion_models/qwen_image_2.1_turbo_int8_convrot.safetensors"], llm: [QI, "text_encoders/qwen3vl_8b_int8_convrot.safetensors"], llm_vision: [QV, "mmproj-Qwen3VL-8B-Instruct-Q8_0.gguf"], vae: [QI, "vae/qwen_image_2.1_vae_bf16.safetensors"] }, args: ["--cfg-scale", "1.0", "--steps", "8", "--sampling-method", "euler", "--sigmas", "qwen-image-2.1-turbo"] },
    ],
  },
  {
    id: "z-image-turbo",
    name: "Z-Image Turbo",
    about: "Small and fast. Good photos in 8 steps. Best choice for 6-12 GB GPUs.",
    license: "Apache-2.0",
    edit: false,
    variants: [
      { id: "q8", label: "High quality (Q8)", min_vram_gb: 12, files: { diffusion: [ZG, "z_image_turbo-Q8_0.gguf"], llm: [Q4, "Qwen3-4B-Instruct-2507-Q8_0.gguf"], vae: [ZT, "split_files/vae/ae.safetensors"] }, args: ["--cfg-scale", "1.0", "--steps", "8"] },
      { id: "q4", label: "Small (Q4)", min_vram_gb: 6, files: { diffusion: [ZG, "z_image_turbo-Q4_K.gguf"], llm: [Q4, "Qwen3-4B-Instruct-2507-Q4_K_M.gguf"], vae: [ZT, "split_files/vae/ae.safetensors"] }, args: ["--cfg-scale", "1.0", "--steps", "8", "--offload-to-cpu"] },
    ],
  },
];

async function json(url) {
  const r = await fetch(url, { headers: { "user-agent": "imagegen-cc-catalog" } });
  if (!r.ok) throw new Error(`${url}: ${r.status}`);
  return r.json();
}

const trees = new Map();
async function hfFile(repo, path) {
  if (!trees.has(repo)) trees.set(repo, await json(`https://huggingface.co/api/models/${repo}/tree/main?recursive=1`));
  const f = trees.get(repo).find((x) => x.path === path);
  if (!f?.lfs?.oid) throw new Error(`missing ${repo}/${path}`);
  return { repo, path, size: f.size, sha256: f.lfs.oid };
}

for (const r of recipes) {
  for (const v of r.variants) {
    for (const [role, [repo, path]] of Object.entries(v.files)) v.files[role] = await hfFile(repo, path);
    v.download_bytes = Object.values(v.files).reduce((s, f) => s + f.size, 0);
  }
}

const rel = await json(`https://api.github.com/repos/leejet/stable-diffusion.cpp/releases/tags/${SD_TAG}`);
const pick = (re) => {
  const a = rel.assets.find((x) => re.test(x.name));
  if (!a) throw new Error(`no asset ${re}`);
  return { url: a.browser_download_url, size: a.size, sha256: a.digest.replace("sha256:", "") };
};
const engines = {
  tag: SD_TAG,
  assets: {
    "windows-rocm": pick(/bin-win-rocm-.*-x64\.zip$/),
    "windows-vulkan": pick(/bin-win-vulkan-x64\.zip$/),
    "windows-cuda": pick(/bin-win-cuda12-x64\.zip$/),
    "windows-cuda-runtime": pick(/^cudart-sd-bin-win-cu12-x64\.zip$/),
    "windows-cpu": pick(/bin-win-cpu-x64\.zip$/),
    "linux-rocm": pick(/Linux-.*-x86_64-rocm-.*\.zip$/),
    "linux-vulkan": pick(/Linux-.*-x86_64-vulkan\.zip$/),
    "linux-cpu": pick(/Linux-.*-x86_64\.zip$/),
    "macos-metal": pick(/Darwin-.*-arm64\.zip$/),
  },
};
const catalog = { engines, rocm: ROCM, recipes };
mkdirSync(join(root, "resources"), { recursive: true });
writeFileSync(join(root, "resources/catalog.json"), JSON.stringify(catalog, null, 1) + "\n");
console.log(`catalog: ${recipes.length} recipes, ${recipes.reduce((s, r) => s + r.variants.length, 0)} variants, engine ${SD_TAG}`);
for (const r of recipes) for (const v of r.variants) console.log(`  ${r.id}/${v.id}: ${(v.download_bytes / 1e9).toFixed(1)} GB, min ${v.min_vram_gb} GB VRAM`);
