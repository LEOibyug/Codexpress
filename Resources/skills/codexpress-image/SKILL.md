---
name: codexpress-image
description: Generate or edit raster images through the local Codexpress image gateway using a logged-in official account's plan allowance. Use when the user chooses Codexpress or its local image endpoint, including while Codex chat uses another provider.
---

# Codexpress Image

Use the bundled `scripts/image.py` with Python 3. It reads the local URL and independent Key from `~/.codexpress/image-client.json`; `CODEXPRESS_IMAGE_CONFIG` can select another configuration file. Never print the Key or include it in prompts, code examples, command arguments, or logs. OAuth credentials stay in Codexpress.

The user must keep Codexpress running and have an enabled, logged-in official source. This route consumes that account's general plan allowance. Do not switch to a paid API or another image service when it fails.

To learn or check the connection, read this skill and optionally run `python3 scripts/image.py check`. Do not generate a test image merely to verify setup.

For a requested image, preserve the user's subject, text and constraints, then choose:

```sh
python3 scripts/image.py generate --prompt "The requested image description" --out /absolute/project/output/image.png
python3 scripts/image.py edit --image /absolute/input.png --prompt "The requested changes; preserve the remaining content" --out /absolute/project/output/edited.png
```

Resolve the script relative to this `SKILL.md`, including when it is inside `Codexpress.app/Contents/Resources/skills/codexpress-image`. Do not assume a particular application installation directory.

- Default model: `gpt-image-2`. `--size` accepts `auto` or supported dimensions such as `1024x1024`, `1536x1024`, and `1024x1536`; `--quality` accepts `auto`, `low`, `medium`, `high`.
- Repeat `--image` for up to five PNG/JPEG/WebP references. A local file is sent as inline image data; remote image URLs and masks are not supported.
- Output is PNG. Choose a new output filename; the script refuses to overwrite existing files. `--n` produces numbered siblings when requesting multiple variants.
- Use a workspace output path for project assets. Inspect the saved image when a viewer is available and report its absolute path or display it inline.
- The script never retries generation automatically. After a timeout, disconnect or unknown result, explain that the request may have consumed allowance and let the user decide whether to resubmit. Do not loop on errors.

Other Images API clients can use the URL and Key from the same private configuration. They must support a custom base URL, `/images/generations` or `/images/edits`, `gpt-image-2`, PNG/base64 output, and disabling automatic retries. Do not modify unrelated global `OPENAI_API_KEY` settings.
