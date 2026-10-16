# imagegen-cc

A Claude Code plugin that lets Claude generate and edit images. It runs image models on your own GPU, connects to your ComfyUI, or uses any OpenAI-compatible image API.

## Install

In Claude Code:

```
/plugin marketplace add Master0fFate/imagegen-cc
/plugin install imagegen-cc@imagegen-cc
```

Or from a shell:

```
claude plugin marketplace add Master0fFate/imagegen-cc
claude plugin install imagegen-cc@imagegen-cc
```

Then run `/imagegen-cc:setup`. A setup page opens in your browser.

Works in the Claude Code CLI and in local sessions of the Claude desktop app (Code tab).

## Setup page

- Shows your GPU, VRAM, RAM and free disk.
- Lists the models that fit your GPU and installs one with a single click: engine, model files and checksums.
- Reuses model files you already have. Give it a folder (for example your ComfyUI `models` folder) and files with the same name and size are linked instead of downloaded.
- Connects an online image service (base URL, model, API key) or your own ComfyUI.
- Tests each backend with one real picture.
- Removes everything: engine, downloaded models and saved keys.

## Using it

Ask Claude for a picture. Claude writes the prompt, calls the tool and checks a small preview.

- "Make a hero image of a lighthouse at dusk for the landing page and put it in `public/`."
- "Make the mug in `assets/mug.png` matte black."

Tools:

| Tool | Purpose |
|---|---|
| `generate_image` | New image from a prompt |
| `edit_image` | Edit images or use them as references |
| `list_backends` | Configured backends and output folder |
| `open_setup` | Open the setup page |

Pictures are saved to `Pictures/imagegen-cc/YYYY-MM/` and copied into the project only when asked.

## Local models

| Model | Sizes | Minimum VRAM | License |
|---|---|---|---|
| Qwen-Image 2.1 | int8 18 GB, Q4 10.7 GB, Q3 9.7 GB | 20 / 12 / 8 GB | Apache-2.0 |
| Qwen-Image 2.1 Turbo | int8 18 GB | 20 GB | Apache-2.0 |
| Z-Image Turbo | Q8 11.2 GB, Q4 6.7 GB | 12 / 6 GB | Apache-2.0 |

Local generation uses [stable-diffusion.cpp](https://github.com/leejet/stable-diffusion.cpp). The engine is picked automatically:

| GPU | Engine |
|---|---|
| NVIDIA (Windows) | CUDA, then Vulkan |
| AMD | ROCm, then Vulkan |
| NVIDIA or Intel (Linux), Intel (Windows) | Vulkan |
| Apple Silicon | Metal |
| None | CPU |

The engine starts on the first request, keeps the model loaded, and stops after 10 minutes without requests to free VRAM.

## Data and privacy

- Plugin data (engine, downloaded models, settings) lives in `~/.claude/plugins/data/` and is deleted when the plugin is uninstalled.
- API keys are encrypted with a key held in the system password store (Windows Credential Manager, macOS Keychain, Secret Service). Without a password store, keys are kept in a file readable only by you.
- The setup page listens on `127.0.0.1` only and requires a random token.
- Nothing is sent anywhere except to the backends you configure, Hugging Face (model downloads), GitHub (engine downloads) and AMD's package index (ROCm runtime files).

## Building

Requires Rust (stable) and Node.js 22.

```
cargo build --release
node scripts/check.mjs
node scripts/pack.mjs
node scripts/smoke.mjs --mcpb dist/release/imagegen-cc.mcpb
```

`pack.mjs` writes an installable plugin and a local marketplace to `dist/`:

```
claude plugin marketplace add ./dist
claude plugin install imagegen-cc@imagegen-cc-local
```

`scripts/catalog.mjs` regenerates `resources/catalog.json` with pinned file sizes and SHA-256 hashes.

## License

MIT
