# Future Work

Known improvement areas, mostly identified during the July 2026 decoder audit
(differential stress testing against libheif plus adversarial spec review).
None of these block current correctness: the decoder passes pixel-for-pixel
against `heif-dec` on the full default corpus (libheif samples + ente
fixtures + generated stress corpus).

## 1. Fuzzing

The crate decodes fully untrusted input in production but has no fuzz target.
The differential harness only proves correct pixels for *valid* files; it
says nothing about panics, hangs, or unbounded allocation on *invalid* ones —
several such bugs (integer overflows, unbounded `Vec::with_capacity` from
attacker-controlled lengths) were found by manual review and fixed, and
coverage-guided fuzzing finds this class mechanically.

Suggested setup: a `cargo-fuzz` target calling
`decode_bytes_to_rgba_with_guardrails` with tight guardrails, seeded from
`.heic-test-assets/libheif/fuzzing/data/corpus` and
`.heic-test-assets/stress-corpus`. The newly supported paths (10/12-bit,
4:2:2, 4:4:4, lossless) are the least battle-tested against hostile input.
Longer term, OSS-Fuzz integration would run it continuously.

## 2. Keep expected-failure accounting tight

`scripts/heic_tests.sh verify` now distinguishes pixel comparisons from
expected no-oracle cases. Files the libheif validator cannot decode are still
run through the Rust decoder as a robustness smoke check and counted as
`EXPECTED_VALIDATOR_FAIL`; known unsupported Rust codecs/features are counted
as `EXPECTED_RUST_FAIL`.

Keep those allowlists explicit. If a future libheif build starts decoding an
old no-oracle file, the harness should either compare pixels or require a
specific expected Rust failure. If upstream adds new fuzz corpus failures,
classify them deliberately instead of reintroducing broad skips.

## 3. Memory usage

Pre-existing, unchanged by the audit, measured July 2026:

- Per-file peak RSS is ~2.9x libheif's on the decode bench — the harness
  threshold is 3.0x, so an unrelated refactor could trip it.
- Concurrent decode (bench-stream, 10 workers) peaks near 3 GiB, roughly
  double the harness's stated 1536 MiB aspiration (that threshold is
  informational; nothing passes `--enforce`).

A memory-diet pass on the decode pipeline is the biggest resource win
available if decoding runs server-side.

## 4. Grid-typed and size-mismatched alpha channels

The last known *silent* wrong-output class: an auxiliary alpha image that is
itself a `grid` item (tiled image with alpha), or an alpha plane whose
dimensions differ from the primary image, is currently ignored — the output
renders fully opaque with no error. libheif decodes grid alpha via its
generic item decode and nearest-neighbor-scales mismatched planes. Either
implement (route aux items through the grid decode path; scale mismatched
planes) or at least fail loudly.

## 5. Continuous integration

Nothing runs the regression net automatically. A CI job doing `cargo test`
plus `scripts/heic_tests.sh verify --quick` would enforce it; the runner
needs `cmake`, `ffmpeg`, and network access for the libheif/fixtures setup
(see TESTING.md). One caveat: color-conversion float kernels are FMA-fused
to match clang's contraction in ARM libheif builds; an x86 runner comparing
against an x86-built libheif without FMA may see rare ±1-LSB pixel
differences.

## Non-goals unless real files appear

These HEVC features are rejected with explicit "unsupported" errors
(previously they silently produced garbage). None appear in real-world HEIC
photo output; implement only if a legitimate file shows up: multi-slice
pictures, in-bitstream tiles (PPS tile grids — HEIF grids are unaffected),
IPCM, range-extension coding tools (RDPCM, persistent Rice, etc.),
differing luma/chroma bit depths, and non-zero per-tile `irot`/`imir`
transforms on grid tiles.
