# screen-translate

[日本語](README.md) | **English**

A tray app for Windows that reads any part of your screen and replaces the text with its translation, right where it is, the way Google Translate does with images. It translates into Japanese, easy Japanese, English, Korean and Chinese. Everything runs on your PC; nothing you translate leaves it.

Built with PLaMo.

## Getting started

Windows 11 is required. There is nothing to install, and the Visual C++ runtime comes in the zip. Unpack the zip anywhere and run `screen-translate.exe`. A short guide opens the first time, where you choose a language and a translation model.

Settings and models are kept in the `data` folder next to the exe, so deleting the folder removes the app. To update, unpack a new zip over the old one. If the folder is not writable, as in Program Files, the app uses a `screen-translate` folder in `%APPDATA%` and `%LOCALAPPDATA%` instead. If you use autostart, turn it off in the settings before deleting the folder.

Because it is a downloaded file, SmartScreen may warn you the first time you run it. Choose "More info" and then "Run anyway", or tick "Unblock" in the zip's properties before unpacking. On a PC with Smart App Control turned on, you need to do the latter.

## Usage

| Key | Action |
| --- | --- |
| Ctrl+Alt+T | Select a region and translate it once |
| Ctrl+Alt+R | Select a region and keep translating it live. Stop with Ctrl+Alt+X, or by pressing this key twice |
| Ctrl+Alt+X | Stop and clear the translation |
| Ctrl+Alt+Q | Show the original while held |
| Ctrl+Alt+C | Copy the translation to the clipboard. The result appears next to the cursor |

You can change the keys in the settings, assign the middle and side mouse buttons, reset each key to its default, or disable it. Hotkeys keep working while the settings window is open, except while you are choosing a key.

Drag to select a region; a green line shows around it as you drag. Click without dragging to select the whole window under the white outline. Esc or a right click cancels, and any translation already running carries on.

While you select, a bar at the top of the screen switches the source, the target, and live mode on or off.

To add a region, press Ctrl+Alt+T or Ctrl+Alt+R again during a translation and drag with Shift held. This lets you translate separate places at once, such as subtitles and a chat window. Without Shift, the new region replaces what was running.

Each region keeps the mode it was selected in: live if the bar's live switch was on, one-shot if it was off. When the page under a one-shot region changes, only that region's translation goes away. Translations do not follow a moving window when there are several regions.

"Pause" in the tray menu stops the hotkeys and all translation, which helps when they clash with the keys of a game. Live translation picks up again when you resume.

Pressing the same hotkey again on the selection screen translates the previous region again. After turning a page, that makes retranslating a matter of pressing Ctrl+Alt+T twice.

The translation is laid over the original paragraph by paragraph, matching its size, background, color and weight. If it runs long, a single line spreads into the empty space to its right, and the text shrinks only if that is not enough. Icons, input borders, images and everything else that is not text stay visible. On a photo or gradient, a one-shot translation paints out just the letters and draws outlined text on top; a live translation uses a solid box, because the background keeps moving. Numbers, code and anything else that would come out the same are left alone.

A selected region is tied to the window under it. The translation moves with the window, is redone when the window is resized, and hides when it is minimized. A one-shot translation disappears on its own when the screen under it changes substantially.

The app never takes the keyboard focus, so your active window stays active. Clicks and scrolling pass through the translation to the window below.

## Settings

Double-click the tray icon to open the settings. The window uses WebView2, which comes with Windows 11.

- Translate from and into: the same choices as the bar
- Style: automatic, polite or plain. Automatic follows the tone of the original
- Glossary: words that must get a fixed translation, used only when the original contains them
- Read Korean: an extra download that adds Hangul
- Translation model: choose, download, delete, load, unload
- Run on: automatic, GPU only, CPU only
- GPU memory limit: the most GPU memory the engine may use in automatic mode
- CPU threads: the number of performance cores by default, which you can lower
- Hotkeys: a key for each of the five actions, each with reset and disable
- Green frame: a thin line just outside the region, or just inside it for a window that fills the screen. It can also flash when a translation finishes. Both are off by default
- Translation font, bold and strength: lowering the strength lets the screen show through
- History: recent translations, with search and copy. Kept in memory only and gone when the app exits. It can be turned off
- Start with Windows: off by default
- Language: Japanese or English, following Windows at first

The default, automatic, runs what fits in GPU memory on the GPU and the rest on the CPU. With enough free memory everything goes on the GPU, and on a PC without a GPU everything runs on the CPU. If GPU memory is tight, the lighter 0.8B model runs faster.

There is no RAM limit. RAM use depends on the size of the model and how much of it is on the GPU, so to reduce it, choose a lighter model or put more on the GPU.

Starting the app a second time opens the settings of the one already running. If the app crashes, the cause is written to `crash.log` in the `data` folder.

## Languages

The app reads Latin-script languages (English, French, German, Spanish and so on), Chinese and Japanese. To read Korean, download the OCR model (13 MB) in the settings. After that, Korean is translated automatically like the other languages.

Text already in the target language is left alone. With easy Japanese as the target, for example, foreign text becomes easy Japanese and the Japanese on screen stays as written. Setting the source to "Japanese too" rewrites that Japanese into easy Japanese as well. Easy Japanese is a setting the model card does not list (`ja(easy)`), but the model's template and PFN's CLI use it.

With English as the target, text in plain Latin letters counts as English and is not translated. Russian, Arabic and Thai cannot be read by the OCR model, so they cannot be translated.

## Models

| Model | Size | Notes |
| --- | --- | --- |
| PLaMo 3 Translate 0.8B | 0.6 GB | Light. About twice as fast as 2B, with more mistranslations |
| PLaMo 3 Translate 2B | 1.8 GB | Standard |
| PLaMo 3 Translate 8B | 5.3 GB | High quality |
| PLaMo 3 Translate 31B | 19.5 GB | Best quality |

Other models work too. Put a GGUF file in `data\models` and it appears in the settings. Models whose file name does not start with `plamo-3-translate` are asked to translate through their own chat template.

A cancelled download keeps its partial file, and the next download resumes from it. "Delete" removes it if you no longer want it.

## Measurements

These are from a PC with an RTX 4070 Ti and a Core i7-13700F, running PLaMo 3 Translate 2B on the GPU.

| Item | Value |
| --- | --- |
| One sentence, from selecting the region to the translation showing | 0.1 to 0.3 s |
| Live update, from the text changing to the translation showing | About 0.2 s |
| From the screen changing to the old translation going away | About 0.04 s |
| A 2000×1077 settings screen with about 40 paragraphs | About 1 s to the first result, about 2 s in all |
| First translation after starting the app | About 3 s |
| App memory, idle | 9 MB |
| App memory, working | About 90 MB |
| Engine memory, model loaded | About 380 MB |
| GPU memory, model loaded | About 2.1 GB |

The OCR models leave memory after 60 seconds without use, and the engine stops after 15 minutes. Unloading stops the engine, which frees all of its memory, on the GPU as well.

Run modes and engines compare as follows, translating 10 paragraphs with 4 requests at a time.

| Setup | Time | RAM | GPU memory |
| --- | --- | --- | --- |
| Automatic (default; all on the GPU on this PC) | 0.68 s | 360 MB | 2.15 GB |
| Automatic, GPU memory limit 1.5 GB | 3.8 s | 1.0 GB | 1.5 GB |
| Automatic, GPU memory limit 1 GB | 6.4 s | 1.5 GB | 1.0 GB |
| CPU only (8 threads) | 7.5 s | 2.2 GB | 0 |
| For reference: llama-server + CUDA | 0.6 s | 650 MB | 2.35 GB |
| For reference: Ollama (CUDA) | 0.66 s | 1.5 GB | 2.3 GB |

The more layers go to the CPU, the slower it gets, so use the GPU memory limit only when you are short of memory. A limit also adds about 3 seconds to loading, to work out the split.

## How it works

- The screen is captured with GDI's BitBlt. A separate thread looks at it every 0.033 seconds and passes any change to OCR. It keeps watching during OCR, so stale translations go away at once.
- OCR is PP-OCRv6 small, run on the CPU by rten, with the models embedded in the exe. If the Korean model is installed, lines the main model is unsure of are read by it too, and the result is used if it is Hangul. The thread count follows the PC's performance cores. The ink in each detected box is examined row by row to find the extent of the text, its background and text colors, and its weight.
- Translation starts llama-server and talks to it over HTTP. Paragraphs are grouped into about 240 characters, one paragraph per line, sent 4 at a time, and shown line by line as they arrive. Paragraphs in a group give each other context, and the group before is passed along as reference.
- Window moves arrive through WinEvent, and the translation moves without any OCR.
- The overlay is a topmost window with a see-through color.

## Limitations

- Live translations do not appear in screenshots or recordings, so that OCR never reads its own output. One-shot translations do, though once in a while one will not. With several regions, one-shot regions behave like live ones in this respect.
- In live mode, text on a photo or gradient is covered by a solid box.
- Vertical and rotated text cannot be read.
- Regions with a long side over 1,600 pixels are scaled down before reading, which makes small text harder to read. Several regions count as the area around them all, so regions far apart are affected too.
- A one-shot translation goes away when 8% or more of the region's pixels change, so a small scroll may leave it up. Use live mode to follow the screen.
- Games in exclusive fullscreen and copy-protected video are not supported.

## Building

You need Rust (MSVC).

```bash
cargo build --release
```

The engine is looked for at `engine\llama-server.exe` next to the exe, then inside an Ollama installation. To make the release zip, run the following. It fetches the official llama.cpp build (Vulkan) and packages it with the app.

```bash
powershell -ExecutionPolicy Bypass -File scripts/package.ps1
```

To check how the overlay looks without touching the screen, draw it onto an image.

```bash
RENDER_IN=in.png RENDER_OUT=out.png cargo test render_picture -- --ignored --nocapture
```

## License

The app's source code is under the MIT License. [NOTICE.md](NOTICE.md) covers what is bundled and the models, and the full texts are in the `licenses` folder.

PLaMo 3 Translate is provided under the [PLaMo Community License](https://plamo.preferredai.jp/info/plamo-community-license-en). The model is in neither this repository nor the releases; downloading it from the settings means you accept that license. **If you want to use this at work, read the [PLaMo Community License](https://plamo.preferredai.jp/info/plamo-community-license-en).** Business use on a work PC counts as commercial use, whether or not it runs locally. If the most recent fiscal year's revenue, affiliates included, is 1 billion yen or less, you must submit PFN's registration form; above that, you need a separate agreement. If you publish translations, state that they were generated by PLaMo.
