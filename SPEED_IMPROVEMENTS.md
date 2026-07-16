# HEIC decoder speed improvements

Date: 2026-07-16

Branch: `speed_improvements`

## Goal

Improve HEIC decoding performance for Ente's Rust ML indexing pipeline without
changing decoded pixels, orientation, color handling, public behavior, or code
readability. ONNX Runtime inference is deliberately outside this work.

The implementation work is intentionally split into four independently
reviewable and revertible commits:

1. compile diagnostic counters and CTU tracking out of production builds;
2. decode independent HEIF grid tiles with bounded parallelism while preserving
   deterministic validation and paste order;
3. provide a direct RGB8 decode path so RGB-only consumers do not materialize
   RGBA and then discard alpha;
4. add exact ARM NEON kernels for profiled HEVC hot paths.

## Evidence and prioritization

The optimized iPhone 15 Pro pipeline benchmark covers 14 representative images
with face and CLIP indexing enabled. Its summed release-mode medians are:

| Stage | Time | Share of Rust-controlled non-inference time |
|---|---:|---:|
| Decode | 3,256.4 ms | 98.2% |
| Preprocessing | 49.1 ms | 1.5% |
| Alignment and postprocessing | 9.0 ms | 0.3% |

The five HEIC fixtures account for 3,021.6 ms of decode time. Improving ML
preprocessing and postprocessing cannot materially change indexing latency;
the decoder is the meaningful Rust-side opportunity.

Alternative decoder experiments reinforce this conclusion:

| Decoder | HEIC decode sum | HEIC speedup | Full-corpus reduction |
|---|---:|---:|---:|
| Current decoder | 3,021.6 ms | 1.00x | - |
| ImageIO with current fallback | 1,846.9 ms | 1.64x | 30.3% |
| `heic` 0.1.6 with parallel grids | 1,875.5 ms | 1.61x | 29.7% |
| `libheif-rs` with libde265 | 1,625.3 ms | 1.86x | 35.6% |

The parallel `heic` experiment was roughly even with the current decoder on
ordinary single-image HEICs but improved the two grid/problem fixtures by
3.16x and 2.67x. This makes grid scheduling the largest demonstrated
algorithmic opportunity in the current pure-Rust implementation.

The alternatives passed Ente's existing downstream face and CLIP thresholds,
but that is weaker than byte identity. Changes within this repository must use
the current scalar/sequential implementation as an exact oracle wherever
possible.

## Existing strengths to preserve

- The decoder has pixel-for-pixel differential tests against `heif-dec` over
  libheif samples, Ente camera fixtures, and a generated HEVC stress corpus.
- The image integration lazily decodes into the buffer supplied by
  `image::ImageDecoder::read_image`, avoiding an extra owned RGBA handoff.
- Grid output is pasted directly into the final canvas and can apply supported
  orientation transforms while pasting.
- The decoder exposes guardrails for pixel counts and input buffering.
- x86-64 transform, residual, dequantization, and 4:2:0 color conversion paths
  already have scalar fallbacks suitable as exact SIMD test oracles.

## 1. Compile out production diagnostics

### Finding

The default `std` feature currently enables debugging work in normal release
decodes even though `DEBUG_TRACE` is false and `SE_TRACE_LIMIT` is zero:

- every syntax-element trace site increments a global atomic counter;
- every decoded CTU reads the CABAC position, locks a global mutex, and appends
  to the tracker vector;
- large-coefficient tracking locks the same mutex;
- residual and NxN diagnostic counters increment in hot paths.

The collected tracker is printed only when `DEBUG_TRACE` is true, so this work
has no production consumer.

### Recommendation

Introduce an opt-in `decoder-tracing` Cargo feature and compile all tracing
state and calls to no-ops when neither that feature nor tests are enabled.
Correctness checks that affect decode behavior should remain separate from
diagnostic logging.

### Measurement

Run a randomized release A/B on the same local corpus and command before and
after the change. Record the exact commands, selected files, aggregate time,
and per-file medians here when implemented.

## 2. Bounded deterministic grid-tile parallelism

### Finding

HEIF grid tiles are independent coded image items, but both the planar grid
decode and direct RGBA grid paths decode them sequentially. Real problem
fixtures contain dozens of coded items, leaving most device cores idle while
decode dominates latency.

### Recommendation

- Add a small internal bounded worker scheduler rather than unbounded
  thread-per-tile execution.
- Decode tiles in parallel, attach their original row-major indices, and
  consume results in index order for deterministic error selection,
  validation, and paste behavior.
- Decode the first tile synchronously because it establishes geometry, layout,
  bit depth, range, matrix, and output allocation.
- Bound concurrency by available parallelism, remaining tile count, and a
  conservative decoded-tile memory budget.
- Keep one sequential path available to tests as the exact oracle.
- Do not parallelize output writes unless non-overlap is mechanically proven;
  ordered paste is cheap and avoids unsafe aliasing.

The preferred memory shape is a bounded sliding window of decoded tiles, not a
`Vec` containing every decoded tile in a large panorama.

## 3. Direct RGB8 output

### Finding

Ente's ML pipeline ultimately calls `DynamicImage::into_rgb8`. The HEIC image
hook advertises and fills RGBA8, after which `image` allocates RGB8, copies
three channels, and discards alpha. For a 55.9-megapixel panorama, the RGBA and
RGB allocations are approximately 224 MB and 168 MB respectively, excluding
YUV planes and decoder scratch.

The measured post-decode RGBA-to-RGB copy is only tens of milliseconds across
the benchmark corpus, so the primary benefit is lower peak memory and safer
parallelism rather than a large standalone latency reduction.

### Recommendation

- Add first-class `DecodedRgbImage`/`DecodedRgbPixels` APIs and direct RGB8
  caller-buffer functions.
- Share color conversion and transform logic with RGBA rather than maintaining
  a second decoder pipeline.
- Use direct RGB only when alpha is absent or the caller explicitly requests
  alpha discard. Preserve RGBA behavior for alpha-bearing images.
- Keep the `image` hook's existing RGBA contract for compatibility unless the
  consuming integration can explicitly request the RGB path.
- Verify direct RGB byte-for-byte against dropping alpha from the existing
  final transformed RGBA8 result.

## 4. Exact ARM NEON kernels

### Finding

Explicit decoder SIMD is currently x86-64 AVX2. AArch64 builds dispatch to the
scalar implementations for inverse transforms, dequantization, residual add,
and the fixed-point 4:2:0 YCbCr-to-RGB kernel. These operations are natural
NEON candidates and are relevant on every supported iPhone.

### Recommendation

Profile an AArch64 release build first and implement kernels in measured order.
Likely candidates are:

1. residual add and clamp;
2. 4:2:0 YCbCr-to-RGB interleaving;
3. dequantization;
4. 8x8/16x16 inverse transforms, followed by larger transforms only if the
   profile justifies them.

Use `core::arch::aarch64` intrinsics behind compile-time architecture gates and
retain runtime dispatch conventions consistent with the existing x86 code.
Every kernel needs randomized scalar-vs-NEON unit tests covering saturation,
rounding, bit depths, odd widths, unaligned buffers, and scalar tails. Full
corpus output must remain pixel-identical.

## Accuracy gates

Each commit must pass, in increasing scope:

1. `cargo fmt --check`;
2. `cargo clippy --all-targets --all-features -- -D warnings`;
3. `cargo test --all-features`;
4. quick pixel verification during development;
5. the repository's complete `scripts/heic_tests.sh all` suite after all four
   changes.

The final Ente integration must additionally preserve face detections,
landmarks, blur scores, face embeddings, and CLIP thresholds. Pet models were
not enabled in the original benchmark, so pet-enabled parity should be covered
separately before treating those results as validated.

The HEIC corpus should include all EXIF orientations, `irot`/`imir`, ICC and
Display P3 content, 8/10/12-bit inputs, alpha, odd dimensions, ordinary images,
large grids, and malformed-input fallback behavior.

## Benchmark discipline

- Build Rust and the application in optimized release mode; reject debug or
  accidentally unoptimized artifacts.
- Warm model sessions before measured indexing passes.
- Remove synchronous stage logging for final wall-clock comparisons.
- Randomize decoder/fixture order where practical and report medians plus
  ranges or confidence intervals.
- Measure peak RSS alongside latency, particularly for grid parallelism.
- Cap thread counts explicitly so results are reproducible and nested
  parallelism cannot oversubscribe the device.

## Deferred opportunities

- Cross-image pipelining may improve library throughput, but should follow the
  decoder memory reduction because several simultaneous full-resolution HEICs
  can consume gigabytes.
- ThinLTO and fewer codegen units are safe release-build experiments after the
  larger algorithmic work.
- WPP row parallelism is substantially more complex than independent HEIF grid
  tiles and should be considered only after profiles show remaining HEVC-core
  limits.
- The CR2 Dart-to-JPEG fallback is both slow and inaccurate, but is separate
  from this HEIC decoder work.

## Implementation results

### Production diagnostics

Normal builds no longer contain syntax-element, residual, or NxN atomic
counters, CABAC-position collection, per-CTU mutex/vector tracking, or
large-coefficient tracking. The diagnostics remain available through the
opt-in `decoder-tracing` feature.

The repository's full decode benchmark was run before and after with 12 files
and five release runs per file. Raw summed Rust averages moved from 19.408 s to
18.816 s (3.1% lower), but the external validator moved from 19.942 s to
19.220 s (3.6% lower) during the same sequential runs. The normalized
Rust/validator ratio therefore changed from 0.973x to 0.979x, showing that the
raw improvement was ambient machine variation rather than a defensible patch
effect.

A tighter alternating comparison used independently built pre-change and
post-change release binaries:

| Fixture | Pre-change median | Post-change median | Difference |
|---|---:|---:|---:|
| 55.9 MP grid panorama | 9.31 s | 9.29 s | -0.2% |
| Ordinary 8.6 MP HEIC | 1.65 s | 1.67 s | +1.2% |

The corresponding averages were 9.392 s versus 9.334 s for the panorama and
1.658 s versus 1.670 s for the ordinary image. These differences are within
run-to-run noise. Both binaries produced identical output SHA-256 hashes.

Conclusion: compiling diagnostics out is worthwhile production cleanup and
removes global synchronization and unused allocations, but it did not produce
a measurable end-to-end speedup on this Mac benchmark.

Results for the remaining improvements will be added as they land.
