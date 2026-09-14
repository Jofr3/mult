# Local changes to fnug-vt100 0.15.2

Source: the published crates.io `fnug-vt100` 0.15.2 package, under its bundled
MIT license. Keep this copy at the same version until an upstream replacement
preserves these behaviours.

The sources are formatted by the repository's `cargo fmt --all` gate; formatting
changes in `attrs.rs`, `row.rs` and `term.rs` are mechanical.

- B26: `Grid::scroll_up` retains rows when the scrolling region starts at row
  zero, including when its bottom margin leaves space for a prompt. Codex's
  inline renderer uses that layout; requiring a full-height scrolling region
  discarded its conversation output. Regions starting below the top and the
  alternate screen still retain no history. The unused `scroll_region_active`
  helper was removed.

Regression coverage lives in Mult's PTY and mouse-routing tests. Existing
wide-character resize workarounds remain in `src/pty.rs`; this patch does not
fix those separate upstream defects.
