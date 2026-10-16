---
name: setup
description: Open the imagegen-cc setup dashboard to install a local image model, connect an image API, or connect ComfyUI. Use when the user runs /imagegen-cc:setup or asks to set up, change, test, or remove image generation.
---

# Image generation setup

1. Call the `open_setup` tool from the `imagegen-cc` MCP server.
2. Tell the user, in one or two sentences, that the dashboard is open in their browser and that they can:
   - install a model that fits their graphics card (the best fit is highlighted),
   - point it at a folder of model files they already have,
   - connect an online image service or their own ComfyUI,
   - press Test to make one real picture.
3. If the tool returns an address instead of opening the browser, give the user that address.
4. Do not ask the user for API keys in the chat. Keys go into the dashboard only.
5. When the user says setup is done, call `list_backends` to confirm a backend exists.
