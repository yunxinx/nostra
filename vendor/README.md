# Vendored RaTeX crates

This directory contains the RaTeX 0.1.14 crates that Nostra patches for bounded
font memory ownership:

- `ratex-font-loader`
- `ratex-unicode-font`
- `ratex-svg`

They were copied from the crates.io 0.1.14 packages. Cargo's VCS metadata records
upstream commit `08cae05377938391117913ca4f278e6a3ffb6a8a` from
<https://github.com/erweixin/RaTeX>. Each crate declares the upstream MIT
license in its `Cargo.toml`.

The remaining RaTeX parser, layout, types, font metrics, and embedded font
packages stay on crates.io 0.1.14. This keeps the patch surface limited to font
discovery/loading and standalone SVG glyph rendering.
