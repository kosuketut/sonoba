# Sonoba

[日本語](README.md) | **English**

This English version was translated from the Japanese README by PLaMo 3 Translate 2B, the model this app uses, and then corrected where it was wrong.

This is a tray app for Windows. It performs OCR on any area of the screen, replacing text with translations. You can choose from Japanese, easy Japanese, English, Korean, or Chinese as the target. Like Google Translate's image translation, it overlays the translation on top of the original text. All processing is completed entirely on your PC.

Built with PLaMo.

## Getting started

The supported OS is Windows 11. No installation is required. The Visual C++ runtime is included in the zip file, so it works without installing it separately. Extract the zip file to your preferred folder and launch `sonoba.exe`. The first time you run it, a guide opens: select the display language, agree to the license, and choose a translation model. Translation is not available until you have agreed.

Settings and models are saved in the `data` folder next to the exe. Deleting the entire folder will also delete the app. To update, overwrite the folder with the contents of a new zip file. If you place the app in a location it cannot write to, such as Program Files, they are saved in the `Sonoba` folders in `%APPDATA%` and `%LOCALAPPDATA%`. If you are using auto-start, disable this in the settings before deleting the folder.

Since this is a downloaded file, you may encounter a SmartScreen warning upon the first launch. Select "Run anyway" under "More info," or check "Unblock" in the zip properties before extracting. On PCs where Smart App Control is enabled, this step is required before extraction.

## How to Use

| Key | Action |
| --- | --- |
| Ctrl+Alt+T | Select a region and translate once. |
| Ctrl+Alt+R | Select a region and continue translating in real time. To stop, press Ctrl+Alt+X or press it twice in succession. |
| Ctrl+Alt+X | Stop translating and delete the translation |
| Ctrl+Alt+Q | View the original only while pressing |
| Ctrl+Alt+C | Copy the translation to the clipboard. The result appears next to the cursor. |

You can change the keys in the settings. The middle and side mouse buttons can also be assigned. Each key can be reverted to its default or disabled. Hotkeys are only disabled while a key is selected in the settings screen.

Select a region by dragging. While dragging, a green line appears outside the region. Clicking without dragging selects the entire window within a white frame. You can undo the selection by pressing Esc or right-clicking. Once undone, the previous translation will continue.

While selecting a region, a bar appears at the top of the screen. You can toggle the source, target, and live settings on or off here.

While translating, press Ctrl+Alt+T or Ctrl+Alt+R again, then drag while holding Shift to add a region. You can simultaneously translate distant areas, such as subtitles and the chat field. If you don't hold Shift, the current translation will stop and be replaced by the new region.

Each region operates in the mode selected for that selection. When "live" is on in the bar, the selected region is translated in real time; when off, the selected region is translated as a one-shot translation. For one-shot translations, only the translation for that specific region disappears when the page changes. When multiple regions are selected, they will not move with the window.

You can stop both the hotkey and translation using "Pause" from the system tray's right-click menu. Use this when it conflicts with game keys. Live translation will resume from where it left off once restarted.

Press the same hotkey again on the region selection screen to translate the same region as before. To re-translate after turning a page, simply press Ctrl+Alt+T twice.

Translations are overlaid on top of the original text, paragraph by paragraph, with font size, background color, text color, and boldness matched to the original. When the translation is long, if it spans one line, it expands into the empty space on the right; if that is still insufficient, the text is reduced in size. Non-text elements, such as icons, input field borders, and images, remain visible. For text over photos or gradients, in a one-shot translation, only the text is erased with the surrounding color, and the translated text is overlaid with a border. In live mode, since the background moves, these are covered by a solid-colored square. Elements whose translation does not change, such as numbers or code, are not replaced.

The selected region is linked to the window below it. Moving the window moves the translation along; resizing it retranslates; and minimizing it hides it. For one-shot translations, the text automatically disappears when the region's screen changes significantly.

The active window remains unchanged, as neither the selection nor the translation takes keyboard focus. Clicking or scrolling will move through the translation to reach the window below.

## Settings

Double-click the system tray icon to open Settings. The Settings screen is displayed using WebView2, which comes standard with Windows 11.

- Source and target: Same settings as the bar used to select the region
- Style: Automatic, polite, plain. Auto matches the tone of the original.
- Glossary: A list of words for which you want a fixed translation. It is used only when the word appears in the original text.
- Read Korean: Becomes available with an additional download
- Translation model: Select, download, delete, load, unload
- Execution Method: Automatic, GPU Only, CPU Only
- GPU Memory Limit: When set to Automatic, select the maximum GPU memory used by the translation engine.
- CPU Threads: When set to Auto, the number of performance cores is used. You can reduce this to your preferred number.
- Hotkeys: Keys for each of the five operations. Each key can be reset or disabled.
- Green border: Displays a thin line outside the translation region. For full-screen windows, it displays an inner line. You can also display a thick line for a brief moment when translation is complete. Both are disabled by default.
- Translation font, bold, strength: Lowering the strength makes the original screen visible through.
- History: A list of recent translations. Search and copy available. Stored only in memory and disappears when the app is closed. Can also be turned off.
- Autostart: Starts when Windows boots. Disabled by default.
- Display language: Japanese or English. Set to match Windows language the first time.

The default "Auto" runs the portion in GPU memory on the GPU and the remainder on the CPU. If there is enough free space, everything runs on the GPU; on PCs without a GPU, it runs solely on the CPU. When GPU memory is low, switching to the lighter 0.8B option will make it run faster.

There is no RAM limit. Since RAM usage is determined by model size and the amount of data loaded onto the GPU, you can reduce usage by choosing a lighter model or increasing the amount of data loaded onto the GPU.

Launching the app again will open the settings for the running instance. If it crashes unexpectedly, the cause is written to the `crash.log` file in the `data` folder.

## Supported Languages

The system can read Latin-script languages (such as English, French, German, and Spanish), Chinese, and Japanese. For Korean, you can read the text by downloading the OCR model (13MB) from the settings, and it will automatically translate it just like other languages.

It does not touch characters written in the target language. For example, if the target is "easy Japanese," it translates only the foreign language into easy Japanese, leaving the Japanese text on the screen as is. If the source is set to "Japanese too," the Japanese text on the screen is also rewritten into easy Japanese. Easy Japanese is a specification not found in the model card (`ja(easy)`) and is used by the model template and the PFN CLI.

When the target language is English, sentences containing only Latin characters are treated as English and are not translated. Russian, Arabic, and Thai cannot be translated because the OCR model cannot read them.

## Model

| Model | Size | Notes |
| --- | --- | --- |
| PLaMo 3 Translate 0.8B | 0.6GB | Lightweight. About twice as fast as 2B, but more mistranslations. |
| PLaMo 3 Translate 2B | 1.8GB | Standard |
| PLaMo 3 Translate 8B | 5.3GB | High quality |
| PLaMo 3 Translate 31B | 19.5GB | Highest Quality |

Other models can also be used. Placing GGUF files in `data\models` will make them appear in the settings list. Models whose filenames do not start with `plamo-3-translate` are asked to translate through their own chat template.

If you cancel the download, files left unfinished will be retained, and the next download will pick up from where they left off. If you no longer need them, you can delete them using "Delete."

## Measurements

These are the values when running PLaMo 3 Translate 2B on the GPU of an RTX 4070 Ti and Core i7-13700F PC. Values for 0.8B are given only where measured.

| Item | Value |
| --- | --- |
| Translation of 1 sentence (from defining the region to displaying it) | 0.1–0.3 s |
| Live updates (2 lines of subtitles, from text change to display) | Approx. 0.8 s (0.77–0.87 s for 2B, 0.66–0.83 s for 0.8B). Most of it is OCR, at 0.5–0.65 s. |
| Translating one figure | 2B is about 2 seconds, 0.8B is about 1 second |
| From the moment the screen changes until the old translation disappears | about 0.04 seconds |
| 2000×1077 settings screen, approx. 40 paragraphs | About 1 second for the first display, about 2 seconds in total |
| First translation after launching the app | about 3 seconds |
| App memory (idle) | 9MB |
| App memory (running) | Approx. 90MB |
| Translation engine memory (model loaded) | Approx. 380MB |
| GPU memory (model loaded) | 2B is about 2.1GB, 0.8B is about 0.65GB |

The OCR model is unloaded after 60 seconds without use, and the translation engine is stopped after 15 minutes without use. Since unloading means stopping the engine, both the memory and GPU memory are completely freed up.

The differences between the execution method and the engine are as follows (translation of 10 paragraphs, 4 simultaneous).

| Setup | Time | RAM | GPU memory |
| --- | --- | --- | --- |
| Automatic (Default, all GPU on this PC) | 0.68 seconds | 360MB | 2.15GB |
| Automatic, GPU memory limit 1.5GB | 3.8 seconds | 1.0GB | 1.5GB |
| Automatic, GPU memory limit 1GB | 6.4 seconds | 1.5GB | 1.0GB |
| CPU only (8 threads) | 7.5 seconds | 2.2GB | 0 |
| Reference: llama-server + CUDA | 0.6 seconds | 650MB | 2.35GB |
| Reference: Ollama (CUDA) | 0.66 seconds | 1.5GB | 2.3GB |

The more layers you pass to the CPU, the slower it becomes, so use the GPU memory limit only when memory is short. Setting a limit also extends the loading time by about 3 seconds to account for the allocation calculation.

## How It Works

- The screen is captured using GDI's BitBlt. A separate thread scans the screen every 0.033 seconds and passes any changes to OCR. Since it can detect changes even during OCR, old translations are quickly cleared.
- OCR is executed on the CPU using PP-OCRv6 small via rten. The model is embedded in the exe. If the Korean model has been downloaded, only lines where reading confidence is low are also read using the PP-OCRv5 Korean model; if the text is in Hangul, that model is used instead. The number of threads is adjusted to match the number of performance cores on the PC. The ink within the detection frame is examined line by line to determine the character-only region, background color, text color, and boldness.
- Translation is requested via HTTP by launching llama-server. Paragraphs are grouped into about 240 characters, one paragraph per line, sent four at a time, and displayed as each line is completed. The grouped paragraphs form the context, and the original text of the preceding group is also provided for reference.
- Window movement is detected via WinEvent, and the translation is moved without performing OCR.
- The overlay is the frontmost window with a transparent color.

## Limits

- Live translations will not appear in screenshots or recordings. This is to prevent OCR from reading its own translations. One-shot translations will appear, though they may occasionally fail to do so. When multiple regions are selected, the one-shot translation region will also not appear.
- In live mode, text over photos or gradients is covered by a solid-colored square.
- Vertically written and rotated text cannot be read.
- Regions where the long side exceeds 1,600 pixels are scaled down before being read, making small characters harder to read. When there are multiple regions, the size is calculated by enclosing all of them, so distant regions are treated the same.
- In one-shot translations, the text disappears if 8% or more of the pixels change. It may remain with slight scrolling. Use live mode to make it follow the screen.
- Exclusive full-screen games and copy-protected videos are not supported.

## Build

Rust (MSVC) is required.

```bash
cargo build --release
```

The app looks for the translation engine at `engine\llama-server.exe` next to the exe file; if it's not found, it uses the one bundled with Ollama. The following command creates the distribution zip. It retrieves and includes the official build (Vulkan version) of llama.cpp.

```bash
powershell -ExecutionPolicy Bypass -File scripts/package.ps1
```

To check the display without touching the screen, draw the translation on the image and save it.

```bash
RENDER_IN=in.png RENDER_OUT=out.png cargo test render_picture -- --ignored --nocapture
```

## License

The source code for this app is licensed under the MIT License. The licenses for the bundled materials and models are summarized in [NOTICE.md](NOTICE.md). The full texts are in the `licenses` folder.

PLaMo 3 Translate is provided under the [PLaMo Community License](https://plamo.preferredai.jp/info/plamo-community-license-en), and the model is not included in this repository or the distribution. Until you agree to it in the first-run guide, you cannot translate or download a model. **When using this for business purposes, please read the [PLaMo Community License](https://plamo.preferredai.jp/info/plamo-community-license-en).** Using it for work on a workplace PC also constitutes a commercial use, regardless of whether it runs locally. If your revenue for the most recent fiscal year (including affiliates) is 1 billion yen or less, you must submit the PFN registration form; if it exceeds 1 billion yen, a separate contract is required. When publishing translation results, please clearly indicate that they are PLaMo-generated.
