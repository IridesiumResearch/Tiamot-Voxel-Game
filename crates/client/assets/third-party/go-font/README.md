# Go Mono

`Go-Mono.ttf`, from the Go project's font family, used for every glyph the
client's HUD draws.

- **Licence: BSD-3-Clause**, in `LICENSE` beside this file, copyright 2009 The
  Go Authors.
- **Source:** <https://github.com/golang/image>, `font/gofont/ttfs/Go-Mono.ttf>`.
- Vendored rather than fetched at build time: a build that reaches the network
  is a build that fails when the network does, and a font is 170 KiB that
  changes approximately never.

## Why this font, specifically

Because its licence is boring, which is the entire requirement.

egui ships default fonts under `OFL-1.1 AND Ubuntu-font-1.0`. Both are free
licences and the argument that an aggregated font is not a derivative of the
program is a good one — but the Ubuntu Font Licence is not FSF-recognised as
GPL-compatible, so it is an argument rather than a fact, and this project is
GPL-3.0-only with enforcement hygiene as a stated goal (charter rule 17). A
dependency whose licensing needs an argument is a dependency that will need it
again at the worst possible moment.

BSD-3-Clause needs no argument. It is already on `deny.toml`'s allow-list, it is
compatible with everything, and the per-crate exception that used to carry
egui's fonts is gone rather than reworded.

Monospace suits the HUD's job — coordinates, chunk counts, timings — where a
proportional font makes columns of numbers jump around as they change. It is
mapped to both of egui's families, so nothing has to choose.
