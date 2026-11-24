# HEIC Correctness and Performance Tests

This crate intentionally does not track image corpora, external validator source
trees, validator build products, or helper binaries. The test harness keeps all
generated files under `.heic-test-runs/`, and local assets under
`.heic-test-assets/`; both are gitignored.

The harness mirrors the correctness and performance checks used in `libheic-rs`.
It uses libheif only as an external validator and optional corpus source. The
crate does not use libheif source code or link to libheif.

The default corpus has four parts:

1. the libheif checkout's sample/test/fuzz images,
2. the HEIC fixture corpus from
   <https://github.com/ente/test-fixtures> (`media/heic/v1/files`) — real
   camera files that have caught regressions the libheif corpus misses, and
3. the locally generated HEVC stress corpus (once generated, see below) —
   synthetic encodes exercising rare syntax paths that neither of the other
   corpora reaches: 10/12-bit, 4:2:2, 4:4:4, lossless, transform skip,
   custom scaling lists, extreme QPs, small CTUs, 1-CTB-wide WPP, NxN intra
   partitions, and odd/tiny picture sizes. Several of these paths carried
   silent-corruption bugs before this corpus existed, and
4. the locally generated incremental corpus (once generated, see
   [Incremental bounded decoder](#incremental-bounded-decoder)) — synthetic
   crops, orientations, grids, and colour metadata, including clean apertures
   with odd origins.

Changes to the HEIC decoder should be regression-tested with a default-corpus
`verify` run, which covers all four. None of the corpora are ever checked
into this repository.

CI runs this on every pull request (`.github/workflows/tests.yml`): a `lint`
job (`cargo fmt --check`, clippy, `cargo test`) and a `verify` job that
performs the full default-corpus correctness pass, including stress- and
incremental-corpus generation. The workflow pins the Rust toolchain plus the
libpng, libheif, and ente test-fixtures commits it fetches; bump those pins in
the workflow to move the CI toolchain, validator, or corpus forward
deliberately.

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

## Bounded decoder

CI also compares `decode_bounded` with the strict libheif primary-image oracle
described in the next section. Build the oracle and generate the incremental
corpus as shown there, then run:

```bash
cargo build --release --locked --features incremental-experiment --bins
ruby scripts/incremental/generate-bounded.rb .heic-test-assets/incremental-corpus
ruby scripts/incremental/verify-bounded.rb \
  --require-pass .heic-test-assets/bounded-corpus/pass-odd-width-crop.heic \
  .heic-test-runs/incremental-oracle/primary-oracle \
  target/release/incremental-allocation target/release/incremental-compare \
  .heic-test-assets/libheif/examples \
  .heic-test-assets/libheif/tests/data \
  .heic-test-assets/libheif/fuzzing/data/corpus \
  .heic-test-assets/ente-test-fixtures/media/heic/v1/files \
  .heic-test-assets/stress-corpus \
  .heic-test-assets/incremental-corpus \
  .heic-test-assets/bounded-corpus
```

`generate-bounded.rb` wraps the incremental corpus's 256×256 encode into the
ignored `.heic-test-assets/bounded-corpus`, or into an optional output folder.
These files are kept out of `incremental-corpus` because `verify.rb` requires
every file there to decode with the incremental experiment. The script refuses
an output folder that overlaps the incremental corpus, or that is not empty
and lacks the `.generate-bounded` marker it writes, and it only replaces the
`pass-*` files in its own folder, deleting any `refuse-*` files left by older
versions. The files cover odd crops with rotations that keep 4:2:0 chroma, the
Display P3 and BT.2020 nclx cases, and transforms for which libheif converts
to 4:4:4: odd-origin crops and odd-sized rotations and mirrors, on one tile and
on 2×2 and 3×2 grids whose seams fall inside the converted area. Rotations and
mirrors that convert are followed by a crop that trims one pixel from every
side, because the pinned libheif 1.21.2 interpolates the converted image's
edge rows and columns differently from the fixed kernel the decoders use. CI
passes every `pass-*` file, `IMG_0983.HEIC` and `1718_rotate_90_cw.HEIC` with
`--require-pass`, which fails the run unless that file passes at both sizes.

Every HEIC, HEIF and AVIF file under the given paths is decoded at max side
6000 and 97 with a 512 MiB budget (`BOUNDED_BUDGET_MIB` overrides it). The
runner denies any allocation that would take the live heap past the budget.
Each successful decode is compared with the oracle's image after the
comparator applies the Exif orientation reported by
`exif_orientation_hint_from_path`, converts an embedded ICC profile or the
item's BT.2020 or Display P3 nclx primaries to sRGB, and area-averages to the
output size. The `rgb8-rounding` gate described below applies. The Exif value
comes from the decoder's own parser and the colour conversion from moxcms;
the comparator's orientation mapping and area average are independent.

Refusals with `Unsupported`, `LimitExceeded` or `MemoryBudgetExceeded` are
counted but do not fail the run. The run fails on a crash, a timeout, an
oracle timeout or crash, `AllocationFailed` for any file, any other error on a
file the oracle decodes, a successful decode of a file the strict oracle
rejects, a dimension or pixel mismatch, or a required file that does not pass.
Results and a summary are written under `.heic-test-runs/bounded`;
`BOUNDED_TEST_ROOT` overrides the folder.

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
Grid tiles are hidden items, so `heif-dec` writes only the primary image.
Once the corpus exists, default `verify` runs also compare each of its `.heic`
files exactly against `heif-dec`. Regenerate a corpus made before tiles were
hidden: `heif-dec` writes visible tiles first, and `verify` would compare the
primary decode against a tile. Cases whose grid tiles carry their own odd
clean aperture are written to the `tile-crop` subdirectory. The incremental
and bounded decoders refuse tile crops. Cases that rotate or mirror odd-sized
or 4:2:2 images are written to the `orientation` subdirectory. The bounded
decoder reproduces libheif's 4:4:4 conversion for the 4:2:0 cases there and
refuses the 4:2:2 ones, which its HEVC decoder does not support. The
incremental runner below only receives the top-level files, and default
`verify` and the bounded comparison cover both subdirectories.

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

For a clean aperture that splits chroma samples, libheif converts the image
to 4:4:4 with bilinear chroma before cropping. If the coded colour is limited
range, it also converts the image through RGB to full-range 4:4:4, because
that conversion targets a default full-range profile. The regular decoders
reproduce both steps for primary items, grids, and grid tiles. Default
`verify` compares them exactly against `heif-dec` on `odd-short`, `oriented`,
`odd-tall`, `odd-pipeline`, `odd-width-grid`, `odd-height-grid`, and the
three `tile-crop` cases.

libheif's `irot` and `imir` run the same conversion for some odd-sized 4:2:0
images and for most 4:2:2 rotations (see [API.md](API.md)). The `orientation`
cases cover each rule against `heif-dec`. The 4:2:0 cases rotate or mirror an
odd grid canvas and then apply a `clap` that removes one pixel from every edge,
because the conversion keeps all four edges of the image and the pinned
libheif's 4:2:0 border defect (below) would show on them. One case rotates the
planes before a mirror converts them. The 4:2:2 cases keep their edges, because
the pinned 4:2:2 kernel has no border defect, and include a 180° turn of an
odd width that libheif runs on the subsampled planes without converting.
`odd-height-grid-quarter-turn`, `odd-three-quarter-turn`, and
`chroma422-odd-vertical-mirror` need no conversion and guard against
converting too often. A 4:2:0 primary item reaches an odd size only through a
`clap`, so its converting rotations keep the cropped edges and differ from the
pinned validator there. Unit tests cover those; on such probes the decoder
matched `heif-dec` exactly once the pinned border was modelled.

The pinned libheif has a bilinear chroma-border indexing defect, fixed
upstream by strukturag/libheif commit 215cbdad. The decoders implement the
fixed border. Odd-grid regressions retain an interior aperture so they still
exercise the final chroma neighbor without depending on its erroneous
outer-border conversion. Every supplied input uses the unmodified live strict
oracle; there are no stored corrected PNGs or input-specific comparison
exceptions. This leaves the affected outer-border RGB conversion outside
independent libheif coverage. Do not raise the global tolerance or silently
classify discrepancies as oracle defects.

A separate high-frequency odd-crop probe remains outside the passing corpus:
at side 6000, one green sample is 72 versus the strict oracle's 70, despite
exact native YUV agreement with FFmpeg. The sample lies inside a band, and
the display-conversion cause remains unresolved. The incremental path
converts its bilinear chroma straight to RGB and omits libheif's
limited-range round trip through full-range 4:4:4; that difference has not
yet been confirmed as the cause. The one-value RGB gate is unchanged; this
probe is not counted as a passing input. Preserve it for the decoder
compatibility follow-up.

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
packages a supplied one-IDR Annex-B 8-bit 4:2:0 or 4:2:2 stream into a direct
HEIC or repeated-tile grid; its dimensions must match the stream. `CROP`,
`ROTATION` and `MIRROR` environment variables add fixture transforms without
re-encoding, in the order `TRANSFORM_ORDER` lists them (default
`CROP,ROTATION,MIRROR`).
`GRID=1` creates a single-tile grid, `CANVAS` clips its visible dimensions,
`TILE_CROP` adds a centred clean aperture to every grid tile,
`TILE_NCLX` and `PRIMARY_NCLX` set color metadata, and `REFERENCE_COUNT`
constructs malformed reference-count regression inputs.
The 200 MP photographic fixture used during development was externally
sourced and re-encoded as Main Still Picture Level 8.5, with WPP disabled;
it is not committed and is not a native camera HEIC compatibility claim.
