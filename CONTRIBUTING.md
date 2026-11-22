# Contributing to OchraOS

Thanks for your interest! Read this before opening a pull request.

## Getting started

```
cargo xtask build    # build
cargo xtask run      # boot in QEMU
cargo xtask test     # run tests
```

You need Rust (version pinned in `rust-toolchain.toml`), QEMU and OVMF.

## Code style

- `cargo fmt` and `cargo clippy -- -D warnings` must be clean.
- Small pull requests, each with a test.
- Every `unsafe` block has a `// SAFETY:` comment.
- SPDX licence header at the top of every source file.

## Developer Certificate of Origin (DCO)

Every commit must be signed off by a human (`git commit -s`):

```
Signed-off-by: Your Name <you@example.com>
```

By signing off you certify that you have the right to submit the change under the project's
licence ([developercertificate.org](https://developercertificate.org)).

## AI-assisted contributions

AI-assisted code is welcome, under these rules:

- An AI model **never** signs off. The human who submits the change signs off and takes full
  responsibility for it.
- Add an `Assisted-by:` trailer, e.g. `Assisted-by: Claude:claude-opus-5-5`.

| Kind of change | Trailers | Pull request description |
| --- | --- | --- |
| Human + AI | human `Signed-off-by:` + `Assisted-by:` | welcome: what was written by hand and what by AI, and a short note from the model on why it was done this way |
| Fully AI-written | human `Signed-off-by:` + `Assisted-by:` + label `ai-generated` | required: test report following the project procedures and the reasoning; an extra review by a second person |

These rules apply equally to the project owner.

## Licences

- Kernel: GPL-2.0-only
- Userspace components: MIT OR Apache-2.0
- Artwork: CC BY-SA 4.0

## Conduct

Be kind. The project follows the Contributor Covenant.

## Hall of Fame

Contributors (opt-in) appear in Settings → About → Hall of Fame, ordered by the date of their
first contribution.
