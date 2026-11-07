# Notices

screen-translate itself is under the MIT License (see LICENSE). It uses the following. The full license texts are in the `licenses` folder.

## PLaMo 3 Translate

Built with PLaMo.

The model is not included in this repository or in release packages. The app downloads it from Hugging Face when the user asks, under the [PLaMo Community License](https://plamo.preferredai.jp/info/plamo-community-license-en) ([Japanese](https://plamo.preferredai.jp/info/plamo-community-license-ja)), which the user accepts by downloading it.

## PP-OCRv6

`models/PP-OCRv6_det_small.onnx`, `models/PP-OCRv6_rec_small.onnx` and `models/dict.txt` are PaddleOCR's PP-OCRv6 small models in the ONNX form distributed by RapidOCR, under the Apache License 2.0.

- PaddleOCR: <https://github.com/PaddlePaddle/PaddleOCR>
- RapidOCR: <https://github.com/RapidAI/RapidOCR>

The app downloads PaddlePaddle's official ONNX export of the Korean recognition model when the user asks, also under the Apache License 2.0. `models/dict_korean.txt` is its character list; `models/korean_PP-OCRv5_mobile_rec.onnx` is a copy for the tests and is not in release packages.

- <https://huggingface.co/PaddlePaddle/korean_PP-OCRv5_mobile_rec_onnx>

## llama.cpp

Release packages contain an unmodified Vulkan build of llama.cpp in `engine/`, under the MIT License (`licenses/llama.cpp.txt`), with nlohmann/json (`licenses/llama.cpp-json.txt`) and the LLVM OpenMP runtime (`engine/LICENSE-LLVM-OpenMP`).

- <https://github.com/ggml-org/llama.cpp>

## Microsoft WebView2

The settings window links the WebView2 loader from Microsoft's WebView2 SDK (`licenses/WebView2-SDK.txt`), through webview2-com (`licenses/webview2-rs.txt`).

## Microsoft Visual C++ runtime

Release packages contain `vcruntime140.dll`, `vcruntime140_1.dll` and `msvcp140.dll` from Microsoft's Visual C++ Redistributable in `engine/`, unmodified, under the Microsoft Software License Terms that allow them to be redistributed with an application.

## Rust crates

The app links rten (`licenses/rten.txt`), ureq, serde_json, rayon, windows-sys and webview2-com, among others, under the MIT and Apache-2.0 licenses. `licenses/rust-crates.txt` lists them all with their texts, and `scripts/licenses.ps1` writes that file.
