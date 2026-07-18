# HEIC Decoder: Further Speed Optimization Suggestions

## 1. Parse HEVC NAL units and metadata only once

The current decode path can traverse and parse the same HEVC stream multiple times for metadata extraction, VCL validation, and the actual decode. Grid images repeat this setup for every tile.

Refactor the decoder boundary so a single NAL pass supplies both the decoded frame and parsed metadata. This should reduce RBSP conversion, SPS parsing, temporary allocations, and per-tile setup costs, with the largest benefit expected on tiled images and smaller mobile images.

### Experiment result (2026-07-18): rejected

Implemented a source-only candidate that walked hvcC and payload NALs directly, reused the first parsed SPS across metadata/backend decode, handed the image-hook probe SPS into pixel decode (including the first grid tile), and removed the normalized intermediate stream allocation. The candidate passed formatting, Clippy, all 63 library tests, and the complete validator run: 272 corpus files accounted for, 219 pixel-oracle cases passed, 219 production image-hook parity checks passed, and zero failures.

The 225-file production `image`-crate hook A/B benchmark was nevertheless neutral on every target, with identical output fingerprints:

- Apple Silicon desktop: `0.997307x` (baseline 1141.273 ms, candidate 1144.355 ms; -0.27%).
- Pixel 4 / Android 13: `1.005837x` (baseline 7020.472 ms, candidate 6979.729 ms; +0.58%; thermal status 0 before/after).
- iPhone 11 Pro / iOS 26.5: `0.999721x` (baseline 2312.484 ms, candidate 2313.130 ms; -0.03%).

This did not meet the repeatable 2% improvement gate on either mobile device or desktop, so the implementation was reverted. The result suggests NAL normalization/SPS re-parsing is not a material share of end-to-end production-hook latency on this corpus.

## 2. Avoid materializing cropped YUV planes

Clean-aperture crops and non-default strides currently cause full Y, U, and V plane copies before color conversion. Even a one-pixel crop can therefore add substantial memory traffic.

Represent decoded planes as stride-aware views containing an origin, dimensions, and row stride, then convert the cropped region directly. Alternatively, add a finalizer that converts directly from the decoded frame into the caller's RGB/RGBA buffer. This should lower both latency and peak memory use on mobile devices.

### Experiment result (2026-07-18): rejected

Implemented a coded-image `image`-hook fast path that retained backend-owned planes as validated stride/origin views and converted directly into the caller's RGBA8/RGBA16 buffer. It covered YUV400/420/422/444, full/limited range, 8/10/12-bit content, auxiliary alpha, clean aperture, mirror, and rotation; full-range 8-bit 4:2:0 retained the region SIMD kernel. Grid paths were deliberately left for suggestion 3. The candidate passed 66 library tests, strict Clippy, portability checks, focused adapter/direct parity fixtures, and the complete validator run: 272 corpus files accounted for, 219 pixel-oracle cases passed, 219 production image-hook parity checks passed, and zero failures.

The 225-file production `image`-crate hook A/B benchmark retained identical fingerprints but did not improve end-to-end latency:

- Apple Silicon desktop: `0.994873x` (baseline 1141.117 ms, candidate 1146.998 ms; -0.51%).
- Pixel 4 / Android 13: `0.965463x` (baseline 6718.879 ms, candidate 6959.226 ms; -3.45%; thermal status 0 before/after).
- iPhone 11 Pro / iOS 26.5: `1.001774x` (baseline 2294.987 ms, candidate 2290.924 ms; +0.18%; thermal state nominal throughout).

The implementation was reverted. Avoiding conformance-window plane copies was not a material production-hook win on this corpus, and the roughly 860-line specialized finalizer was not justified—particularly with a clear Android regression.

## 3. Convert grid tiles directly into the final output

Grid tiles are converted into temporary RGBA buffers before being copied or transformed into the destination image.

For identity orientation, add an output-stride-aware NEON conversion path that writes each tile directly into its final destination region. For 90-degree and 270-degree orientations, investigate blocked conversion and transpose operations to retain cache locality. Keep this experiment narrower than the previously rejected general affine-conversion implementation.

### Experiment result (2026-07-18): rejected

Implemented the deliberately narrow production-hook path: opaque, identity-oriented RGBA8 grids converted each validated and clipped YUV tile directly into its final caller-buffer region using the canvas row stride. Full-range 8-bit 4:2:0 kept the AArch64 NEON kernel with strided stores, while the portable scalar converter covered the other 8-bit layouts and ranges. Rotations, mirrors, clean apertures, auxiliary alpha, and RGBA16 stayed on the established temporary-tile fallback. The candidate passed 65 library tests, strict Clippy, all portability builds, focused real-grid parity, and the complete validator run: 272 corpus files accounted for, 219 pixel-oracle cases passed, 219 production image-hook parity checks passed, and zero failures.

The 225-file production `image`-crate hook A/B benchmark retained identical fingerprints and showed small improvements, but none was repeatably above the 2% gate:

- Apple Silicon desktop: `1.012266x` (baseline 1135.645 ms, candidate 1121.884 ms; +1.23%).
- Pixel 4 / Android 13: `1.006170x` (baseline 7055.583 ms, candidate 7012.319 ms; +0.62%; thermal status 0 before/after).
- iPhone 11 Pro / iOS 26.5: `1.019976x` (baseline 2284.174 ms, candidate 2239.440 ms; +2.00%; thermal state nominal throughout). The two interleaved iPhone pairs were only +0.3% and +3.6%, so this borderline mean was not repeatable.

The implementation was reverted. Direct identity-grid conversion reduced work as intended, but it affects too little of the full production-hook corpus to justify the added specialized path under the experiment's acceptance rule.

## 4. Make grid and frame parallelism mobile-aware

Fixed concurrency and use of the global Rayon pool can oversubscribe mobile CPUs, increase memory pressure, and cause thermal throttling—particularly when an application decodes multiple images concurrently.

Expose a concurrency or memory budget, support a caller-provided worker pool, and benchmark conservative mobile defaults such as two to four workers. Prefer ordered streaming of completed tiles so fewer decoded tile buffers need to remain live. Evaluate peak RSS and energy consumption alongside latency.

### Experiment result (2026-07-18): accepted

Replaced the grid decoder's fixed parallel batches with a bounded ordered stream on the existing Rayon pool. Active jobs and completed out-of-order tiles retain per-tile memory permits until row-major consumption, preserving the 64 MiB estimate budget; unestimable tiles drain earlier work and decode synchronously. Validation, errors, panics, and tile paste remain observable in row-major order. The first sub-variant also reduced iOS/Android to four workers, but physical-device testing showed that default was harmful: Pixel full-hook latency regressed 7.28% and iPhone was neutral. The final candidate therefore retained the existing eight-worker ceiling on all targets while keeping bounded streaming.

The final candidate passed formatting, strict Clippy, 76 focused/library/CLI tests, iOS/Android/Wasm portability builds, and two complete validator runs after the scheduler revision: each accounted for all 272 corpus files, passed 219 pixel-oracle cases and 219 production image-hook parity checks, and had zero failures. The 225-file production `image`-crate hook A/B benchmark retained identical fingerprints:

- Apple Silicon desktop: `1.055055x` (baseline 1144.134 ms, candidate 1084.431 ms; +5.51%).
- Pixel 4 / Android 13: the initial revised B/C/C/B set was noisy because of one unusually fast baseline run; four additional reversed C/B/B/C invocations showed `1.022607x` (baseline 7402.952 ms, candidate 7239.291 ms; +2.26%). Across all eight invocations the result was `0.993735x` (-0.63%), with no repeatable regression and thermal status 0 throughout.
- iPhone 11 Pro / iOS 26.5: `1.042723x` (baseline 2315.621 ms, candidate 2220.744 ms; +4.27%). Both interleaved pairs improved by roughly 4%; thermal state was nominal until the tail of the final baseline run.

The ordered streaming implementation was kept. It clears the 2% gate repeatably on desktop and iPhone, reduces batch-barrier idle time, and keeps the existing conservative in-flight memory bound. No direct energy counter was available; thermal state was used as the mobile guardrail.

## 5. Evaluate LTO and profile-guided optimization

The decoder's CABAC and residual paths contain many small helpers and data-dependent branches that may benefit from whole-program optimization and profile feedback.

Benchmark ThinLTO with a single codegen unit in the consuming application, followed by PGO trained on a stratified HEIC corpus. Measure binary size and cold-start performance as guardrails. Apply release-profile settings in the final application workspace because dependency crates cannot reliably control the consumer's Cargo profile.

### Experiment result (2026-07-18): rejected

Implemented an opt-in final-consumer build wrapper rather than a misleading dependency profile. It supported default release, ThinLTO with one codegen unit, and target-specific PGO instrument/train/merge/use builds with compiler/LLVM/target/manifest validation. PGO was trained independently on desktop, Pixel, and iPhone using a fixed 15-file set spanning six real-camera images plus bit-depth, chroma, lossless, scaling-list, transform-skip, and WPP cases; the full 225-file hook corpus remained evaluation-only. Android and iOS raw profiles were generated on their physical devices and retrieved successfully.

The unchanged decoder source passed the complete validator gate: 272 corpus files accounted for, 219 pixel-oracle cases passed, 219 production image-hook parity checks passed, and zero failures. Every default, ThinLTO, and PGO evaluation artifact also produced the identical 225-file hook fingerprint. Full-corpus timings were:

- Apple Silicon desktop: ThinLTO `1.001877x` (default 1101.577 ms, ThinLTO 1099.513 ms; +0.19%); PGO `0.981025x` (1122.884 ms; -1.90%).
- Pixel 4 / Android 13: ThinLTO `0.997096x` (default 6877.490 ms, ThinLTO 6897.521 ms; -0.29%); PGO `1.018734x` (6751.014 ms; +1.87%; thermal status 0). The PGO result remained below the 2% gate.
- iPhone 11 Pro / iOS 26.5: ThinLTO `1.000189x` (default 2154.960 ms, ThinLTO 2154.553 ms; +0.02%; nominal thermal state). The PGO confirmation was `0.984277x` (thermally loaded default 2275.528 ms, PGO 2311.878 ms; -1.57%); its ordering biased the final default slower, so throttling could not be hiding a qualifying PGO win.

Binary size improved despite neutral latency: ThinLTO reduced the final executable by roughly 5.5–12.1% and PGO by 11.0–17.1%, depending on target. A 40-launch tiny-hook guardrail was also neutral at about 3.0 ms median on desktop. Because neither build mode met the production-hook speed gate, the wrapper, training manifest, and documentation were reverted rather than adding maintenance and target-specific training complexity for a size-only benefit.

## 6. Extend SIMD color conversion beyond full-range 8-bit 4:2:0

The NEON fast path is gated to 8-bit, full-range, 4:2:0, opaque matrix content only (`src/lib.rs` `PreparedYcbcrTransform` selection). Everything else — 10-bit HDR photos (`MatrixFullFloat`), limited/video-range YCbCr (`MatrixLimited`), 4:2:2/4:4:4, and alpha-bearing images — falls back to a per-pixel scalar float loop with `fmaf_parity` calls. Recent iPhones (HDR) and many Android cameras produce exactly these formats, so on mobile this is likely the largest untapped win.

Add NEON f32 kernels for the `MatrixFullFloat` and `MatrixLimited` paths. Bit-exact parity with the libheif float oracle is achievable because `fmaf_parity` already uses fused `mul_add` on aarch64 and `vfmaq_f32` is also fused. Also consider ARMv7 NEON and WASM SIMD128 variants, which currently have zero SIMD coverage.

## 7. Batch residual/CABAC micro-optimizations into one coherent change

Several individually positive but sub-gate residual/CABAC experiments were rejected: caller-side bounds-check elimination (x1.007), fused range+transition lookup (x1.008), hoisted/flattened helpers (x1.012), scan-ordered context tables (x1.011), and bypass-bin batching (x1.019–x1.025). The journal shows that combining compatible near-threshold ideas cleared the gate twice before (attempts 33 and 36).

Combine the mutually compatible ones into a single candidate. The most promising component is bypass-bin batching: `decode_bypass_bits` and sign-run decoding still loop one bin at a time; a modest 2–4 bit unroll with one shared refill check should help without repeating the failed 64-bit code-window redesign (x0.906 — do not retry). Avoid large fused lookup tables (2 KiB variant) since cache pressure is worse on mobile.

## 8. Remove full-plane clones in SAO

SAO edge-offset processing clones the entire Y, Cb, and Cr planes (`sao.rs`) to preserve original neighbor values, plus a `deblock_flags` clone per pass. On desktop this measured only ~x1.026 when addressed, but full-plane allocation and memcpy cost relatively more on mobile's smaller caches and lower memory bandwidth, and also costs energy.

Replace the full-plane copies with CTB-local (or row-band) temporary buffers, or a direct source-to-destination pass. Re-measure on a mobile device rather than the desktop host before rejecting.

## 9. Avoid full-plane rotation allocations for non-grid oriented images

Grid images now use accepted affine row-stride placement, but non-grid images with `irot`/`imir` still go through full-plane rotation/mirror functions (`transforms.rs`) that allocate a complete new plane and use per-pixel multiplicative indexing. A prior bounded-band attempt was host-neutral, but the extra full-frame traffic is more expensive on mobile.

Fold orientation into the color-conversion write step with blocked (cache-tiled) transpose for 90/270 rotations, writing directly into the caller's buffer, mirroring the approach that already succeeded for grids.

## 10. Investigate WPP row-parallel reconstruction for non-grid images

WPP entry points are already parsed and per-row CABAC reinitialization works (`ctu.rs`), but CTU rows still decode serially. Grid images get parallelism from tiles; large single-coded-image HEICs do not.

For WPP-encoded streams, decode CTU rows on a wavefront with the standard two-CTU lag. This is a large refactor and only pays off for WPP-encoded files, so first survey how common WPP is in the real-world corpus before committing.
