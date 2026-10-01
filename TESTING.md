# HEIC Correctness and Performance Tests

This crate intentionally does not track image corpora, external validator source
trees, validator build products, or helper binaries. The test harness keeps all
generated files under `.heic-test-runs/`, and local assets under
`.heic-test-assets/`; both are gitignored.

The harness mirrors the correctness and performance checks used in `libheic-rs`.
It uses libheif only as an external validator and optional corpus source. The
crate does not use libheif source code or link to libheif.

The default corpus has three parts:

1. the libheif checkout's sample/test/fuzz images,
2. the HEIC fixture corpus from
   <https://github.com/ente/test-fixtures> (`media/heic/v1/files`) — real
   camera files that have caught regressions the libheif corpus misses, and
3. the locally generated HEVC stress corpus (once generated, see below) —
   synthetic encodes exercising rare syntax paths that neither of the other
   corpora reaches: 10/12-bit, 4:2:2, 4:4:4, lossless, transform skip,
   custom scaling lists, extreme QPs, small CTUs, 1-CTB-wide WPP, NxN intra
   partitions, and odd/tiny picture sizes. Several of these paths carried
   silent-corruption bugs before this corpus existed.

Changes to the HEIC decoder should be regression-tested with a default-corpus
`verify` run, which covers all three. None of the corpora are ever checked
into this repository.

CI runs this on every pull request (`.github/workflows/tests.yml`): a `lint`
job (`cargo fmt --check`, clippy, `cargo test`) and a `verify` job that
performs the full default-corpus correctness pass, including stress-corpus
generation. The workflow pins the Rust toolchain plus the libpng, libheif, and
ente test-fixtures commits it fetches; bump those pins in the workflow to move
the CI toolchain, validator, or corpus forward deliberately.

- pixel-for-pixel PNG comparison against an external `heif-dec` validator
- pixel-for-pixel comparison of the `image` crate integration hook output
  (`ImageReader`/`DynamicImage::from_decoder`) against the direct Rust decode
  for every comparable verifier file, including exact ICC-profile equality
  through the hook decoder's `ImageDecoder::icc_profile`; this reproduces
  Ente's production hook shape, including its explicit guardrails,
  `with_guessed_format`, `Limits::reserve`, and `set_limits`
- embedded ICC colour-profile comparison against the validator's PNG output:
  when `heif-dec` embeds a profile, the Rust PNG must carry byte-identical
  profile data; a Rust-only profile is allowed (the Rust decoder synthesizes
  ICC from nclx colour information, which `heif-dec` does not embed)
- Rust decoder vs external validator decode timing
- bytes vs path ingestion timing
- `image` adapter vs direct decode timing
- path/read concurrent decode timing and RSS

`verify` has explicit accounting for corpus files that cannot produce a
pixel oracle. `EXPECTED_VALIDATOR_FAIL` means libheif failed with an
allowlisted reason, but the Rust decoder was still run as a robustness smoke
check. `EXPECTED_RUST_FAIL` means libheif produced an oracle, but the file
uses a known unsupported Rust codec or feature and failed with the expected
category/message. Any new validator failure, uncategorized Rust failure, or
pixel mismatch is still a hard failure.

## Setup

Put a libheif checkout or symlink under the ignored asset directory:

```bash
mkdir -p .heic-test-assets
ln -s /path/to/libheif .heic-test-assets/libheif
```

Cloning directly into `.heic-test-assets` is also accepted:

```bash
git clone https://github.com/strukturag/libheif.git .heic-test-assets
```

Or point the script at an existing validator/corpus checkout:

```bash
export HEIC_LIBHEIF_SOURCE_DIR=/path/to/libheif
```

The ente test-fixtures corpus needs no setup: the harness fetches it
automatically (a sparse, blobless clone of just the HEIC fixture subtree) into
`.heic-test-assets/ente-test-fixtures` the first time a default corpus is
assembled. If fetching is not possible (e.g. offline), it prints a warning and
runs with the libheif corpus only. To use a pre-existing checkout, clone it
yourself or point the script at it:

```bash
git clone https://github.com/ente/test-fixtures.git .heic-test-assets/ente-test-fixtures
# or
export HEIC_ENTE_FIXTURES_DIR=/path/to/test-fixtures
```

To pick up fixture files added upstream later, refresh the clone:

```bash
git -C .heic-test-assets/ente-test-fixtures pull
```

The stress corpus is generated once (a few minutes of x265 encoding) after
the validator and fixtures are set up:

```bash
scripts/heic_tests.sh gen-stress
```

It lands in the gitignored `.heic-test-assets/stress-corpus/` and is picked
up by default `verify` runs from then on. Regenerate with
`scripts/heic_tests.sh gen-stress --force` (exact bytes may differ across
x265 versions — that is fine, `verify` compares decoded pixels against
`heif-dec` at run time, so any conformant encode of these feature
combinations is a valid test).

Then run:

```bash
scripts/heic_tests.sh all
```

The scripts can build the external validator into
`.heic-test-runs/validator-build` by default. Set
`LIBHEIF_DEC_BIN=/path/to/heif-dec` to reuse an existing validator binary
instead. The only auto-detected validator paths are under `.heic-test-assets/`
and `.heic-test-runs/`; explicit environment variables are left untouched.

Required command-line tools: `cargo`, `cmake`, `ffmpeg`, `ffprobe`, `shasum`,
`awk`, `find`, `sort`, and `/usr/bin/time`.

## Commands

Quick correctness pass:

```bash
scripts/heic_tests.sh verify --quick --require-exts heic,avif
```

Full correctness pass over the configured corpus:

```bash
scripts/heic_tests.sh verify --full --require-exts heic,avif
```

Performance checks:

```bash
scripts/heic_tests.sh bench-decode --full --files 12 --runs 5
scripts/heic_tests.sh bench-ingestion --full --files 12 --runs 5
scripts/heic_tests.sh bench-image --full --files 12 --runs 5
scripts/heic_tests.sh bench-stream --full --files 6 --runs 2 --workers 10 --iterations 4
```

Everything:

```bash
scripts/heic_tests.sh all
```

Passing `--corpus-dir` replaces the default corpus entirely — useful for
reproducing individual files. Default runs (no `--corpus-dir`) cover both the
libheif corpus and the ente fixtures.

Generated reports and PNG artifacts are under `.heic-test-runs/`. Use
`--keep-artifacts` with `verify` when debugging a pixel mismatch.

## Incremental bounded decoder

The opt-in `incremental-experiment` feature is tested separately because
`--all-features` also enables decoder tracing, which disables bounded decode.
The standalone allocation test imposes both live-heap and individual-request
ceilings across decoding and conversion threads. It covers Path/Bytes parity,
source-height growth, odd crops on both axes, grid clipping, tile color-profile
inheritance, threaded grid tiles, and early budget rejection. The malformed
reference fixture uses `.bin` and is tested under a 256 KiB ceiling separately
from the positive `*.heic` oracle comparisons.

The external primary-image oracle is only a test executable; it adds no native
dependency to the Rust decoder. After the existing harness builds libheif,
build and run it with CMake, libpng development files, Ruby, and FFmpeg with
libx265. No incremental image assets are committed. The generator starts
from the existing external libheif corpus and creates the geometry, metadata,
and height-growth cases missing from that corpus in the ignored
`.heic-test-assets/incremental-corpus` directory. All reference PNGs come from
live strict libheif decoding. Its manifest records source, oracle, FFmpeg,
and input identities; encoded bytes can differ across libx265 versions.

```bash
cmake -S scripts/incremental -B .heic-test-runs/incremental-oracle \
  -DHEIF_SOURCE_DIR="$PWD/.heic-test-assets/libheif" \
  -DHEIF_BUILD_DIR="$PWD/.heic-test-runs/validator-build"
cmake --build .heic-test-runs/incremental-oracle --parallel
ruby scripts/incremental/generate.rb \
  .heic-test-runs/incremental-oracle/primary-oracle \
  .heic-test-assets/libheif/fuzzing/data/corpus/colors-no-alpha.heic
cargo test --release --locked --features incremental-experiment --lib --bins --test incremental-memory
cargo test --release --locked --no-default-features --features incremental-experiment --lib --test incremental-memory
cargo build --release --locked --features incremental-experiment --bins
ruby scripts/incremental/verify.rb \
  .heic-test-runs/incremental-oracle/primary-oracle \
  target/release/incremental-allocation target/release/incremental-compare \
  .heic-test-assets/incremental-corpus/*.heic
```

Supply `PNG_PNG_INCLUDE_DIR` and `PNG_LIBRARY` when using a custom libpng,
as the CI workflow does. The runner compares original-size (up to side 6000)
and side-65 outputs, writes identities and metrics under
`.heic-test-runs/incremental`, and fails on any decoder, oracle, geometry, or
pixel-comparison failure. `INCREMENTAL_TEST_ROOT` overrides the output folder.
The generator accepts an optional output directory; set
`INCREMENTAL_FIXTURES_DIR` to that directory for allocation tests. Missing
generated inputs fail the test with setup instructions rather than skipping it.
Every supplied input is required to decode; unsupported inputs do not count
as successful comparisons. CI also exercises the six currently supported
files in the libheif/stress corpus. The separate normal suite keeps its exact
RGB/ICC comparisons and existing expected-failure accounting.

The oracle enables libheif strict decoding and rejects warnings. RGB display
comparisons normalize embedded ICC to sRGB and apply an independent floating
point area average. The `rgb8-rounding` profile allows at most one value per
RGB channel per pixel; alpha remains exact. The `exact` profile allows no
sample changes. Dimensions and raster lengths are always checked. Average
error, PSNR, signed bias and local error are diagnostics, never substitutes
for the per-sample gate. These profiles do not authorize different tone
mapping, resampling filters, or high-bit-depth rounding. Native reconstruction
must retain exact sample checks against an independent decoder. ICC
normalization currently shares moxcms with the implementation; separate qcms
unit tests cover that boundary.

The pinned libheif has a bilinear chroma-border indexing defect. Odd-grid
regressions retain an interior aperture so they still exercise the final
chroma neighbor without depending on its erroneous outer-border conversion.
Every supplied input uses the unmodified live strict oracle; there are no
stored corrected PNGs or input-specific comparison exceptions. This leaves
the affected outer-border RGB conversion outside independent libheif coverage.
Do not raise the global tolerance or silently classify discrepancies as
oracle defects.

A separate high-frequency odd-crop probe remains outside the passing corpus:
at side 6000, one green sample is 72 versus the strict oracle's 70, despite
exact native YUV agreement with FFmpeg. The sample lies inside a band, and
the display-conversion cause remains unresolved. The one-value RGB gate is
unchanged; this probe is not counted as a passing input. Preserve it for
the decoder compatibility follow-up.

For a supplied large supported image, measure allocator requests separately
from uninstrumented decode timing:

```bash
target/release/incremental-allocation bounded image.heic 6000 128
target/release/incremental-allocation bytes image.heic 6000 128
target/release/incremental-bench bounded image.heic
target/release/incremental-bench normal image.heic
```

Run timing trials serially, after warm-up, in alternating order. Normal mode
returns the full raster; bounded mode includes capped output and reduction.
No claim of equal work or universal speedup follows from those timings.
Use a process memory tool separately for RSS. The allocator probe reports
requested heap bytes and excludes the caller's borrowed input allocation.

`incremental-check` is a deliberately unbounded verification tool comparing
native reconstructed samples against the full Rust decoder. For an 8-bit
4:2:0 fixture with no conformance-window crop, provide independently decoded
planar YUV to require exact independent agreement too:

```bash
ANNEX_B_OUTPUT=reference.hevc target/release/incremental-check image.heic
ffmpeg -v error -y -i reference.hevc -frames:v 1 -pix_fmt yuv420p -f rawvideo reference.yuv
REFERENCE_YUV=reference.yuv target/release/incremental-check image.heic
```

This verification tool is not part of the bounded path or its memory evidence.
`capability-inspect` prints coded-item SPS/PPS features. `scripts/incremental/wrap.rb`
packages a supplied one-IDR Annex-B 8-bit 4:2:0 stream into a direct HEIC or
repeated-tile grid; its dimensions must match the stream. `CROP`, `ROTATION`
and `MIRROR` environment variables add fixture transforms without re-encoding.
`GRID=1` creates a single-tile grid, `CANVAS` clips its visible dimensions,
`TILE_NCLX` and `PRIMARY_NCLX` set color metadata, and `REFERENCE_COUNT`
constructs malformed reference-count regression inputs.
The 200 MP photographic fixture used during development was externally
sourced and re-encoded as Main Still Picture Level 8.5, with WPP disabled;
it is not committed and is not a native camera HEIC compatibility claim.
