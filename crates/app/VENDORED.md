# Vendored code ledger

Per ARCHITECTURE.md §1 vendoring policy: every vendored file records its source
repo, revision, license, and all local modifications. Vendored code is frozen at
the recorded rev and refreshed only via a deliberate PR — never as a side effect
of a dependency update.

## src/input/text_input.rs

- **Source**: https://github.com/Augani/adabraka-ui — `src/components/input_state.rs`
- **Revision**: `e158684b23d9cb043fed3989ca252212046dabca` (cloned 2026-08-22)
- **License**: MIT
- **Local modifications**:
  1. Removed `use crate::theme::use_theme;` (adabraka's global theme) and the
     `theme.tokens.muted_foreground` lookup.
  2. Added `placeholder_color` / `cursor_color` / `selection_color` fields to
     `InputState` (neutral defaults in `new()`, `with_colors(..)` builder) so
     colors are injected from the app `Theme` — no hard-coded colors rule.
  3. Replaced the hard-coded cursor `rgb(0x0066ff)` and selection
     `rgba(0x3311ff30)` in `InputTextElement::prepaint` with the injected
     colors.
  4. Added the vendoring header comment.
  5. gpui API drift at our pinned rev (`fd82517a`): `window.focus_next/prev`
     take `cx`; `ShapedLine::paint` takes `TextAlign` + wrap width.
  6. `set_value` now replaces the **entire** content (upstream delegated to
     `replace_text_in_range(None, ..)`, which replaces only the current
     selection — surprising append semantics for a setter).
  7. `#[allow(dead_code)]` on `shake_count` (we don't render adabraka's shake
     animation).
  8. Added `pub fn select_range` (M3): programmatic arbitrary-range
     selection for the inline rename editor's stem preselect — upstream only
     exposes whole-content selection via `select_all`.
  9. Added `pub fn selected_range` (M3): read access to the current
     selection, so callers (and tests) can assert on it — upstream keeps it
     private.
  Everything else (actions, `EntityInputHandler` impl, `InputTextElement`
  shaping/paint, validation, masking, IME marked-range handling) is unmodified.
