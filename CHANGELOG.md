# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### v0.4.0 - 2026-06-05

- **Removed `shaderc` dependency** — shaders are now precompiled to SPIR-V and embedded at build time via `include_bytes!`. No `glslc` or Vulkan SDK required to build the crate.
- **Removed `build.rs`** — no longer needed since shaders are precompiled.
- **Removed `shader.rs`** — SPIR-V constant and `get_spirv_code()` moved to `pipeline.rs`.
- **Added `shader/` directory** — contains GLSL source (`color_convert.comp`), compile script (`compile.sh`), precompiled SPIR-V (`color_convert.spv`), and documentation (`README.md`).
- **Updated README.md** — shader development workflow documented.
