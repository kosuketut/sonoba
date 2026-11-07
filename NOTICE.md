# Notices

screen-translate itself is under the MIT License (see LICENSE). It uses the following.

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

Release packages contain an unmodified Vulkan build of llama.cpp in `engine/`, under the MIT License.

- <https://github.com/ggml-org/llama.cpp>

## Rust crates

The app links rten, ureq, serde_json, rayon, windows-sys and webview2-com, among others, under the MIT and Apache-2.0 licenses. `cargo tree` lists them all.
