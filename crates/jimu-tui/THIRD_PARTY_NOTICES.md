# Third-Party Notices

Copyright xAI. The upstream license file also records Copyright 2023-2026 SpaceXAI.

The terminal rendering engine and UI design in `crates/jimu-tui` are adapted from
[xai-org/grok-build](https://github.com/xai-org/grok-build/tree/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen),
licensed under the Apache License, Version 2.0. The accompanying `LICENSE` file contains
the full Apache 2.0 license text.

Pursuant to Apache License 2.0 §4(b), the following table lists the adapted upstream modules
and summarizes the changes made (principally removing `xai_*` internal product dependencies,
telemetry, disk config hardcoding, and unit tests, switching `syntect` to pure-Rust
`default-fancy`, and wiring the engine behind the harness-agnostic `crate::harness` layer):

| Upstream module (`xai-org/grok-build`) | Local module (`crates/jimu-tui/src/`) | Summary of changes |
| :--- | :--- | :--- |
| `crates/codegen/xai-tty-utils` | `engine/tty.rs` | Migrated child-process TTY detachment and pager environment helpers for `std::process::Command` without `tokio`. |
| `crates/codegen/xai-ratatui-inline` | `engine/inline/` | Full production migration of inline terminal viewport, scrollback emission, synchronized updates, resize purge, and `LinkSpan`. |
| `crates/codegen/xai-ratatui-textarea` | `engine/textarea/` | Full production migration of multiline edit buffer, undo/redo, wrapping, mouse selection, and atomic `TextElement` chips. |
| `crates/codegen/xai-grok-markdown`, `xai-grok-markdown-core` | `engine/markdown/` | Full production migration of streaming checkpoint renderer, table layout, Unicode LaTeX math, Swift syntax asset, and `build.rs`; switched `syntect` to `default-fancy` and gated under `feature = "syntax"`. |
| `crates/codegen/xai-grok-pager-diff` | `engine/diff.rs` | Full production migration of structured diff hunk builder, line numbering, and 500ms timeout; replaced `SearchReplaceEditDetail` with neutral `DiffEdit`. |
| `crates/codegen/xai-grok-pager-render/src/theme/` | `engine/theme/` | Full production migration of all 6 palettes (`grokday`, `groknight`, `tokyonight`, `rosepine`, `oscura`, `terminal_default`), `Auto` polarity detection, OSC11, `system_appearance`, `color_support` quantization, and `md_style`; removed disk config and telemetry coupling. |
| `crates/codegen/xai-grok-pager-render/src/render/`, `glyphs.rs`, `syntax.rs`, `link_opener.rs` | `engine/render/` | Full production migration of VTE terminal output, wrapping, bidi, OSC8, highlight, line utils, safe buffer, scrollbar, glyphs, syntax loader, link opener, and optional `inline-image` overlay; removed `gboom` and video overlays. |
| `crates/codegen/xai-grok-pager-render/src/input/` | `engine/input/` | Full production migration of keyboard normalizer, macOS modifier handling, line editor, mouse handling, and terminal capability probes; removed telemetry scroll logging. |
| `crates/codegen/xai-grok-pager-render/src/clipboard/` | `engine/clipboard/` | Optional (`feature = "clipboard-image"`) image validation, resizing, and base64/MIME helpers without telemetry. |
| `crates/codegen/xai-grok-mermaid` | `engine/mermaid/` | Optional (`feature = "mermaid"`) `MermaidEngine` trait and subprocess timeout isolation without vendored diagram layout crates. |
| `crates/codegen/xai-grok-pager/src/{scrollback,views}` | `app/{blocks,scrollback.rs,views,screen.rs}` | Adapted block visual model (accent bars, auto-folding, grouping, diff/VTE cards, search, block viewer) and interactive modals behind the harness-agnostic `crate::harness::TuiHarness` contract. |
