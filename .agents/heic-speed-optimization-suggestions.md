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

## 4. Make grid and frame parallelism mobile-aware

Fixed concurrency and use of the global Rayon pool can oversubscribe mobile CPUs, increase memory pressure, and cause thermal throttling—particularly when an application decodes multiple images concurrently.

Expose a concurrency or memory budget, support a caller-provided worker pool, and benchmark conservative mobile defaults such as two to four workers. Prefer ordered streaming of completed tiles so fewer decoded tile buffers need to remain live. Evaluate peak RSS and energy consumption alongside latency.

## 5. Evaluate LTO and profile-guided optimization

The decoder's CABAC and residual paths contain many small helpers and data-dependent branches that may benefit from whole-program optimization and profile feedback.

Benchmark ThinLTO with a single codegen unit in the consuming application, followed by PGO trained on a stratified HEIC corpus. Measure binary size and cold-start performance as guardrails. Apply release-profile settings in the final application workspace because dependency crates cannot reliably control the consumer's Cargo profile.

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
