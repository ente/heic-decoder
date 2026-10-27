#!/usr/bin/env bash
# Generate the HEVC stress corpus into .heic-test-assets/stress-corpus.
#
# These files exercise rare HEVC syntax paths that real camera output does not
# cover: 10/12-bit, 4:2:2, 4:4:4, lossless (transquant bypass, level 8.5,
# identity matrix), transform skip, default and custom (asymmetric) scaling
# lists, cu_qp_delta at small quantization groups, extreme QPs, small CTUs,
# deep transform trees, WPP on/off (including 1-CTB-wide pictures), NxN intra
# partitions, disabled loop filters, and odd/tiny picture sizes. Several of
# these paths carried silent-corruption bugs that the standard corpora never
# reached.
#
# The corpus is generated (not checked in) because this repository tracks no
# image assets. Exact bytes may differ across x265 versions; that is fine —
# the verify harness compares decoded pixels against heif-dec at run time, so
# any conformant encode of these feature combinations is a valid test.
#
# Once the directory exists, scripts/heic_tests.sh includes it in the default
# corpus automatically.
#
# Requires the validator build and the ente fixtures corpus, both of which a
# prior harness run sets up (e.g. `scripts/heic_tests.sh verify --quick`).
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ -n "${DEV_SCRATCH_HELPER:-}" && -z "${HEIC_TEST_ROOT:-}" ]]; then
  exec "$DEV_SCRATCH_HELPER" heic "$ROOT_DIR" -- "${BASH_SOURCE[0]}" "$@"
fi
TEST_ROOT="${HEIC_TEST_ROOT:-$ROOT_DIR/.heic-test-runs}"
ASSET_ROOT="${HEIC_TEST_ASSET_ROOT:-$ROOT_DIR/.heic-test-assets}"
LIBHEIF_BUILD_DIR="${LIBHEIF_BUILD_DIR:-$TEST_ROOT/validator-build}"
HEIF_DEC="${LIBHEIF_DEC_BIN:-$LIBHEIF_BUILD_DIR/examples/heif-dec}"
HEIF_ENC="${LIBHEIF_ENC_BIN:-$LIBHEIF_BUILD_DIR/examples/heif-enc}"
FIXTURES_DIR="${HEIC_ENTE_FIXTURES_DIR:-$ASSET_ROOT/ente-test-fixtures}"
PHOTO_HEIC="$FIXTURES_DIR/media/heic/v1/files/IMG_0983.HEIC"

OUT_DIR="$ASSET_ROOT/stress-corpus"
SRC_DIR="$TEST_ROOT/stress-corpus-src"

fail() {
  echo "[gen-stress] ERROR: $*" >&2
  exit 1
}

log() {
  echo "[gen-stress] $*"
}

command -v ffmpeg >/dev/null 2>&1 || fail "Missing command: ffmpeg"
[[ -f "$PHOTO_HEIC" ]] \
  || fail "Fixtures photo not found at $PHOTO_HEIC. Run 'scripts/heic_tests.sh verify --quick' once to fetch the fixtures corpus."
[[ -x "$HEIF_DEC" ]] \
  || fail "heif-dec not found at $HEIF_DEC. Run 'scripts/heic_tests.sh verify --quick' once to build the validator."

if [[ ! -x "$HEIF_ENC" ]]; then
  log "Building heif-enc encoder"
  command -v cmake >/dev/null 2>&1 || fail "Missing command: cmake"
  cmake --build "$LIBHEIF_BUILD_DIR" --target heif-enc --parallel >/dev/null \
    || fail "Could not build heif-enc in $LIBHEIF_BUILD_DIR"
fi

if [[ "${1:-}" != "--force" ]] && [[ -d "$OUT_DIR" ]] \
  && [[ "$(find "$OUT_DIR" -name '*.heic' | wc -l)" -ge 40 ]]; then
  log "Corpus already present at $OUT_DIR (use --force to regenerate)"
  exit 0
fi

rm -rf "$OUT_DIR" "$SRC_DIR"
mkdir -p "$OUT_DIR" "$SRC_DIR"

# --- Source images: one natural photo (from the fixtures corpus) at several
# odd sizes and bit depths, plus synthetic gradients/noise/edges/flat.
log "Generating source images"
"$HEIF_DEC" "$PHOTO_HEIC" "$SRC_DIR/photo_full.png" >/dev/null 2>&1 \
  || fail "heif-dec could not decode $PHOTO_HEIC"
ffmpeg -v error -y -i "$SRC_DIR/photo_full.png" -vf scale=999:1333 "$SRC_DIR/photo_odd.png"
ffmpeg -v error -y -i "$SRC_DIR/photo_full.png" -vf scale=517:389 "$SRC_DIR/photo_small.png"
ffmpeg -v error -y -i "$SRC_DIR/photo_full.png" -vf scale=640:480 -pix_fmt rgb48be "$SRC_DIR/photo16.png"
ffmpeg -v error -y -i "$SRC_DIR/photo_full.png" -vf scale=48:512 "$SRC_DIR/narrow.png"
ffmpeg -v error -y -i "$SRC_DIR/photo_full.png" -vf scale=56:600 "$SRC_DIR/narrow2.png"
ffmpeg -v error -y -f lavfi -i "testsrc2=size=511x255" -frames:v 1 "$SRC_DIR/edges.png"
ffmpeg -v error -y -f lavfi -i "nullsrc=size=512x512" \
  -vf "geq=r='random(1)*255':g='random(2)*255':b='random(3)*255'" -frames:v 1 "$SRC_DIR/noise.png"
ffmpeg -v error -y -f lavfi -i "gradients=size=640x480:n=4" -frames:v 1 -pix_fmt rgb48be "$SRC_DIR/gradient16.png"
ffmpeg -v error -y -f lavfi -i "testsrc2=size=33x17" -frames:v 1 "$SRC_DIR/tiny.png"
ffmpeg -v error -y -f lavfi -i "color=c=0x6a8f5a:size=256x256" -frames:v 1 "$SRC_DIR/flat.png"
rm -f "$SRC_DIR/photo_full.png"

# Custom scaling-list file with asymmetric matrices (x265/HM format). The
# default HEVC matrices are symmetric, so only an asymmetric list catches
# transposed scaling-factor lookups.
python3 - "$SRC_DIR/asym_scaling.txt" <<'PYEOF'
import sys

def mat4():
    return [[16, 18, 22, 27], [18, 22, 27, 33], [25, 30, 38, 47], [36, 43, 54, 68]]

def mat8():
    return [
        [16, 17, 18, 20, 24, 25, 28, 33], [17, 18, 20, 24, 25, 28, 33, 41],
        [19, 22, 26, 28, 32, 36, 43, 52], [21, 24, 28, 32, 36, 43, 52, 61],
        [24, 27, 32, 36, 43, 52, 61, 70], [27, 32, 36, 43, 52, 61, 70, 79],
        [31, 36, 43, 52, 61, 70, 79, 88], [36, 43, 52, 61, 70, 79, 88, 97],
    ]

def flat(m):
    return ",".join(str(v) for row in m for v in row)

lines = []
for name in ["INTRA4X4_LUMA", "INTRA4X4_CHROMAU", "INTRA4X4_CHROMAV",
             "INTER4X4_LUMA", "INTER4X4_CHROMAU", "INTER4X4_CHROMAV"]:
    lines += [f"{name} =", flat(mat4())]
for name in ["INTRA8X8_LUMA", "INTRA8X8_CHROMAU", "INTRA8X8_CHROMAV",
             "INTER8X8_LUMA", "INTER8X8_CHROMAU", "INTER8X8_CHROMAV"]:
    lines += [f"{name} =", flat(mat8())]
for name in ["INTRA16X16_LUMA", "INTRA16X16_CHROMAU", "INTRA16X16_CHROMAV",
             "INTER16X16_LUMA", "INTER16X16_CHROMAU", "INTER16X16_CHROMAV"]:
    lines += [f"{name} =", flat(mat8()), f"{name}_DC =", "16"]
for name in ["INTRA32X32_LUMA", "INTER32X32_LUMA"]:
    lines += [f"{name} =", flat(mat8()), f"{name}_DC =", "16"]
open(sys.argv[1], "w").write("\n".join(lines) + "\n")
PYEOF

ok=0
failed=0

enc() {
  local name="$1"
  local input="$2"
  shift 2
  if "$HEIF_ENC" -o "$OUT_DIR/$name.heic" "$@" "$SRC_DIR/$input" \
      >/dev/null 2>"$OUT_DIR/$name.err" && [[ -s "$OUT_DIR/$name.heic" ]]; then
    ok=$((ok + 1))
    rm -f "$OUT_DIR/$name.err"
    return
  fi
  failed=$((failed + 1))
  echo "[gen-stress] encode failed: $name ($(tail -n 1 "$OUT_DIR/$name.err" 2>/dev/null))" >&2
  rm -f "$OUT_DIR/$name.heic" "$OUT_DIR/$name.err"
}

log "Encoding stress corpus into $OUT_DIR"

# Baseline quality sweep on diverse content
enc q30_photo_odd      photo_odd.png   -q 30
enc q85_vs_photo_odd   photo_odd.png   -q 85 -p preset=veryslow
enc q50_noise          noise.png       -q 50
enc q90_noise          noise.png       -q 90
enc q30_edges          edges.png       -q 30
enc q95_flat           flat.png        -q 95
enc q50_tiny           tiny.png        -q 50
enc q10_photo_small    photo_small.png -q 10

# Lossless (transquant bypass, level 8.5, identity matrix RGB)
enc lossless_photo_small photo_small.png -L
enc lossless_edges       edges.png       -L
enc lossless_noise       noise.png       -L
enc lossless_444_nxn     edges.png       -L -p preset=veryslow

# Chroma formats
enc c422_photo_small   photo_small.png -q 60 -p chroma=422
enc c444_photo_small   photo_small.png -q 60 -p chroma=444
enc c422_edges         edges.png       -q 60 -p chroma=422
enc c444_noise         noise.png       -q 60 -p chroma=444
enc c444_nxn           photo_small.png -q 70 -p chroma=444 -p preset=veryslow -p tu-intra-depth=4

# CTU / transform tree geometry
enc ctu16_photo_small  photo_small.png -q 50 -p x265:ctu=16
enc ctu32_tu4_edges    edges.png       -q 50 -p x265:ctu=32 -p tu-intra-depth=4
enc ctu16_tiny         tiny.png        -q 50 -p x265:ctu=16

# WPP variants (including 1-CTB-wide pictures) and extreme QPs
enc nowpp_photo_small  photo_small.png -q 50 -p x265:wpp=0
enc wpp_narrow         narrow.png      -q 50
enc wpp_narrow2        narrow2.png     -q 65
enc qp51_photo_small   photo_small.png -p x265:qp=51
enc qp0_edges          edges.png       -p x265:qp=0

# Rare residual / coefficient paths
enc tskip_edges        edges.png       -q 50 -p x265:tskip=1
enc tskip_noise        noise.png       -q 40 -p x265:tskip=1
enc culossless_photo   photo_small.png -q 50 -p x265:cu-lossless=1
enc scaling_photo_odd  photo_odd.png   -q 50 -p x265:scaling-list=default
enc scaling_noise      noise.png       -q 50 -p x265:scaling-list=default
enc scaling_custom     photo_small.png -q 45 -p "x265:scaling-list=$SRC_DIR/asym_scaling.txt"
enc nosdh_photo_small  photo_small.png -q 50 -p x265:signhide=0
enc aq3_qg16_photo_odd photo_odd.png   -q 45 -p x265:aq-mode=3 -p x265:qg-size=16
enc nosao_photo_small  photo_small.png -q 50 -p x265:sao=0
enc nodeblock_edges    edges.png       -q 50 -p x265:deblock=0
enc nostrong_photo     photo_small.png -q 50 -p x265:strong-intra-smoothing=0
enc cip_photo_small    photo_small.png -q 50 -p x265:constrained-intra=1
enc rdoq_psy_noise     noise.png       -q 60 -p x265:rdoq-level=2 -p x265:psy-rdoq=10

# High bit depths (16-bit PNG sources)
enc bd10_photo16       photo16.png    -q 60 -b 10
enc bd10_gradient      gradient16.png -q 60 -b 10
enc bd10_lossless_grad gradient16.png -L -b 10
enc bd12_photo16       photo16.png    -q 60 -b 12
enc bd12_gradient      gradient16.png -q 60 -b 12
enc bd10_scaling_grad  gradient16.png -q 50 -b 10 -p x265:scaling-list=default
enc bd10_tskip_photo16 photo16.png    -q 50 -b 10 -p x265:tskip=1

log "Done: $ok encoded, $failed failed, output in $OUT_DIR"
log "The default test corpus now includes this directory automatically."
[[ "$ok" -gt 0 ]] || fail "No files were encoded."
