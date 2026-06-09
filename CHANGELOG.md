# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

- **Added `Bt709LinearToBt2020Pq` color space** — converts linear BT.709 (scRGB, FP16) to BT.2020+PQ via gamut mapping + PQ OETF. Used for HDR games that present with `VK_COLOR_SPACE_EXTENDED_SRGB_LINEAR_EXT`. `sdr_reference_white_nits` controls the tone-mapping scale (80 nits per IEC 61966-2-2).
- **Added `set_sdr_reference_white_nits()`** — dynamically updates the SDR reference white level via push constants without recreating the pipeline.

### v0.4.0 - 2026-06-05

- **Removed `shaderc` dependency** — shaders are now precompiled to SPIR-V and embedded at build time via `include_bytes!`. No `glslc` or Vulkan SDK required to build the crate.
- **Removed `build.rs`** — no longer needed since shaders are precompiled.
- **Removed `shader.rs`** — SPIR-V constant and `get_spirv_code()` moved to `pipeline.rs`.
- **Added `shader/` directory** — contains GLSL source (`color_convert.comp`), compile script (`compile.sh`), precompiled SPIR-V (`color_convert.spv`), and documentation (`README.md`).
- **Updated README.md** — shader development workflow documented.
