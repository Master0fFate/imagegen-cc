---
name: imagegen
description: Generate or edit raster images (photos, illustrations, concept art, textures, icons, mockup assets, placeholders) with the user's own image model. Use when the user asks to create, draw, render, generate, redo, or edit a picture or image, or when a task needs a real bitmap asset.
---

# Image generation

Tools from the `imagegen-cc` MCP server:

| Tool | Use |
|---|---|
| `generate_image` | New image from a text prompt |
| `edit_image` | Change an existing image, or make a new image from reference images |
| `list_backends` | See which models or APIs are configured and where files are saved |
| `open_setup` | Open the setup dashboard in the user's browser |

## When not to use it

- Diagrams, charts, flowcharts, UI layouts: write SVG, HTML, or Mermaid instead.
- Simple icons or logos that must stay sharp at every size: write SVG.

## Steps

1. Write one full prompt (section below). Do not ask the user to write it.
2. Call `generate_image`. Set `width`/`height` only when the use needs a shape (hero banner 1536x640, portrait 832x1216, square 1024x1024). Use multiples of 32.
3. If the image is for the current project, set `save_to` (for example `public/images/` or `assets/hero.png`).
4. Look at the returned preview. If it misses the request, fix the prompt and call again. At most two retries without asking the user.
5. Tell the user the file path. Do not paste base64.

A local model can take one to five minutes per image. The call reports progress. Do not cancel it or start a duplicate.

## Prompt structure

Natural language, in this order, only the parts that matter:

SUBJECT → APPEARANCE → ACTION/POSE → POSITIONS → CAMERA/FRAMING → ENVIRONMENT → LIGHTING → LENS → MATERIALS → STYLE

- Be concrete: "a matte black ceramic mug on a walnut desk, morning window light from the left" beats "a nice mug".
- State text that must appear in quotes: `a sign that says "OPEN"`.
- For several people or objects, describe each one and where it is.
- No keyword lists such as "masterpiece, best quality, 8k".

## Errors

| Message | Action |
|---|---|
| `No image backend is configured` | Call `open_setup` and tell the user to pick a model or service in the dashboard. |
| `Model file ... is missing` | Call `open_setup` so the user can repair the backend. |
| `is unreachable` | The local engine or API is down. Tell the user; do not retry in a loop. |
| `HTTP 401` / `HTTP 403` | The API key is wrong or missing. Call `open_setup` so the user can fix it. Never ask for the key in chat. |
| `Ignored by this OpenAI-compatible backend` | Not an error. The backend does not support seed, steps, or guidance. |
