# Third-party notices

File Explorer is MIT licensed (see [LICENSE](LICENSE)). It also includes or
depends on the work below, each under its own permissive licence. Nothing here
is copyleft, and no GPL code is included — Zed's application crates were read
as reference but never copied (see `docs/ARCHITECTURE.md`).

## Vendored source

### adabraka-ui — MIT

`crates/app/src/input/text_input.rs` is vendored from
[Augani/adabraka-ui](https://github.com/Augani/adabraka-ui) at revision
`e158684b23d9cb043fed3989ca252212046dabca`, with local modifications recorded
in [`crates/app/VENDORED.md`](crates/app/VENDORED.md).

## Bundled assets

### Lucide icons — ISC

The SVGs in `crates/app/assets/icons/` are from
[Lucide](https://lucide.dev) (`lucide-static` v1.44.0), ISC licensed. Each file
keeps its upstream licence comment.

```
ISC License

Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2022 as part
of Feather (MIT). All other copyright (c) for Lucide are held by Lucide
Contributors 2022.

Permission to use, copy, modify, and/or distribute this software for any
purpose with or without fee is hereby granted, provided that the above
copyright notice and this permission notice appear in all copies.

THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES WITH
REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF MERCHANTABILITY
AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR ANY SPECIAL, DIRECT,
INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES WHATSOEVER RESULTING FROM
LOSS OF USE, DATA OR PROFITS, WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE OR
OTHER TORTIOUS ACTION, ARISING OUT OF OR IN CONNECTION WITH THE USE OR
PERFORMANCE OF THIS SOFTWARE.
```

## Principal dependencies

| Crate | Licence | Used for |
|---|---|---|
| [gpui](https://gpui.rs) | Apache-2.0 | the GPU-rendered UI framework |
| `objc2` family | MIT | macOS system APIs (volumes, tags, QuickLook, Launch Services) |
| `notify` | CC0-1.0 / Artistic-2.0 | filesystem watching |
| `trash` | MIT | moving items to the macOS Trash |
| `serde`, `serde_json` | MIT / Apache-2.0 | settings and theme files |
| `image` | MIT / Apache-2.0 | thumbnail decoding fallback |

A full, machine-generated inventory is available with
[`cargo-license`](https://github.com/onur/cargo-license) or
[`cargo-deny`](https://github.com/EmbarkStudios/cargo-deny):

```
cargo install cargo-license && cargo license
```
