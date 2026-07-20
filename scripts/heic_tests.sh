#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TEST_ROOT="${HEIC_TEST_ROOT:-$ROOT_DIR/.heic-test-runs}"
ASSET_ROOT="${HEIC_TEST_ASSET_ROOT:-$ROOT_DIR/.heic-test-assets}"
HELPER_DIR="${HEIC_TEST_HELPER_DIR:-$TEST_ROOT/helper}"
HELPER_BIN_DIR="$HELPER_DIR/target/release"

LIBHEIF_SOURCE_EXPLICIT="${HEIC_LIBHEIF_SOURCE_DIR+x}"
LIBHEIF_DEC_BIN_EXPLICIT="${LIBHEIF_DEC_BIN+x}"
LIBHEIF_SOURCE_DIR="${HEIC_LIBHEIF_SOURCE_DIR:-$ASSET_ROOT/libheif}"
LIBHEIF_BUILD_DIR="${LIBHEIF_BUILD_DIR:-$TEST_ROOT/validator-build}"
LIBHEIF_DEC_BIN="${LIBHEIF_DEC_BIN:-$LIBHEIF_BUILD_DIR/examples/heif-dec}"
LIBHEIF_RECONFIGURE="${LIBHEIF_RECONFIGURE:-0}"
LIBHEIF_REQUIRE_ORACLE_DECODERS="${LIBHEIF_REQUIRE_ORACLE_DECODERS:-1}"
LIBHEIF_PATHS_RESOLVED=0

ENTE_FIXTURES_EXPLICIT="${HEIC_ENTE_FIXTURES_DIR+x}"
ENTE_FIXTURES_DIR="${HEIC_ENTE_FIXTURES_DIR:-$ASSET_ROOT/ente-test-fixtures}"
ENTE_FIXTURES_REPO_URL="${HEIC_ENTE_FIXTURES_REPO_URL:-https://github.com/ente/test-fixtures.git}"
ENTE_FIXTURES_SUBDIR="media/heic/v1/files"

usage() {
  cat <<'EOF'
Usage: scripts/heic_tests.sh <command> [options]

Commands:
  verify           Compare heic_decoder PNG output against an external validator
  bench-decode     Benchmark heic_decoder decode CLI against the validator
  bench-ingestion  Benchmark bytes vs path ingestion
  bench-image      Benchmark image adapter vs direct decode
  bench-stream     Benchmark path/read decode under concurrency
  all              Run full verify plus the standard benchmark set
  gen-stress       Generate the HEVC stress corpus (rare syntax paths) into
                   .heic-test-assets/stress-corpus (--force to regenerate)
  build-helper     Generate and build the local helper binaries only

Common environment:
  HEIC_LIBHEIF_SOURCE_DIR  external validator checkout with examples/tests/fuzz corpus
                           default: .heic-test-assets/libheif
  HEIC_ENTE_FIXTURES_DIR   ente test-fixtures checkout providing the HEIC fixture corpus
                           default: .heic-test-assets/ente-test-fixtures
                           (auto-fetched via sparse git clone when missing)
  HEIC_TEST_ROOT           generated outputs/cache root
                           default: .heic-test-runs
  LIBHEIF_BUILD_DIR        validator CMake build dir
                           default: $HEIC_TEST_ROOT/validator-build
  LIBHEIF_DEC_BIN          existing heif-dec binary to reuse
  LIBHEIF_RECONFIGURE      set to 1 to force validator CMake reconfigure
  LIBHEIF_CMAKE_ARGS       extra CMake args appended to the validator build

If .heic-test-assets itself is a libheif checkout, the script accepts that too.

Use --help after a command for command-specific options.
EOF
}

log() {
  echo "[$1] ${*:2}"
}

fail() {
  echo "[heic-tests] ERROR: $*" >&2
  exit 1
}

require_cmd() {
  local cmd="$1"
  command -v "$cmd" >/dev/null 2>&1 || fail "Missing command: $cmd"
}

is_libheif_source_dir() {
  local dir="$1"
  [[ -f "$dir/CMakeLists.txt" && -d "$dir/examples" && -d "$dir/tests/data" && -d "$dir/fuzzing/data/corpus" ]]
}

first_existing_libheif_source_dir() {
  local candidate
  for candidate in \
    "$LIBHEIF_SOURCE_DIR" \
    "$ASSET_ROOT/libheif" \
    "$ASSET_ROOT"
  do
    if is_libheif_source_dir "$candidate"; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done
  return 1
}

first_existing_heif_dec_bin() {
  local candidate
  for candidate in \
    "$LIBHEIF_DEC_BIN" \
    "$LIBHEIF_BUILD_DIR/examples/heif-dec" \
    "$ASSET_ROOT/libheif-build/examples/heif-dec"
  do
    if [[ -n "$candidate" && -x "$candidate" ]]; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done
  return 1
}

resolve_libheif_paths() {
  [[ "$LIBHEIF_PATHS_RESOLVED" -eq 1 ]] && return

  local detected_source
  if [[ -z "$LIBHEIF_SOURCE_EXPLICIT" ]] && ! is_libheif_source_dir "$LIBHEIF_SOURCE_DIR"; then
    if detected_source="$(first_existing_libheif_source_dir)"; then
      LIBHEIF_SOURCE_DIR="$detected_source"
      log setup "Using validator source: $LIBHEIF_SOURCE_DIR"
    fi
  fi

  local detected_bin
  if [[ -z "$LIBHEIF_DEC_BIN_EXPLICIT" ]] && [[ ! -x "$LIBHEIF_DEC_BIN" ]]; then
    if detected_bin="$(first_existing_heif_dec_bin)"; then
      LIBHEIF_DEC_BIN="$detected_bin"
      log setup "Using validator binary: $LIBHEIF_DEC_BIN"
    fi
  fi

  LIBHEIF_PATHS_RESOLVED=1
}

toml_escape() {
  local value="$1"
  value="${value//\\/\\\\}"
  value="${value//\"/\\\"}"
  printf '%s' "$value"
}

ensure_helper_sources() {
  local root_toml
  root_toml="$(toml_escape "$ROOT_DIR")"
  mkdir -p "$HELPER_DIR/src/bin"

  cat > "$HELPER_DIR/Cargo.toml" <<EOF
[workspace]

[package]
name = "heic_decoder_test_helper"
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
heic_decoder = { path = "$root_toml", features = ["image-integration"] }
image = { version = "0.25", default-features = false, features = ["png"] }
png = "0.18"
EOF

  # Seed the helper workspace with the root lockfile so helper builds resolve
  # the same dependency versions (notably the rav1d git branch) as the crate.
  if [[ -f "$ROOT_DIR/Cargo.lock" ]]; then
    cp "$ROOT_DIR/Cargo.lock" "$HELPER_DIR/Cargo.lock"
  fi

  cat > "$HELPER_DIR/src/bin/heif-png-icc.rs" <<'RS'
use std::borrow::Cow;
use std::fs::File;
use std::io::BufReader;
use std::process::ExitCode;

/// Extracts the decompressed iCCP colour profile from a PNG.
///
/// Exit codes: 0 = profile written to the output path, 3 = the PNG carries
/// no profile, 1 = the PNG could not be read, 2 = usage error.
fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("Usage: {} <input.png> <output.icc>", args[0]);
        return ExitCode::from(2);
    }

    let file = match File::open(&args[1]) {
        Ok(file) => file,
        Err(error) => {
            eprintln!("could not open {}: {error}", args[1]);
            return ExitCode::from(1);
        }
    };
    let reader = match png::Decoder::new(BufReader::new(file)).read_info() {
        Ok(reader) => reader,
        Err(error) => {
            eprintln!("could not parse {}: {error}", args[1]);
            return ExitCode::from(1);
        }
    };
    let Some(profile) = reader.info().icc_profile.as_ref().map(Cow::as_ref) else {
        return ExitCode::from(3);
    };
    match std::fs::write(&args[2], profile) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("could not write {}: {error}", args[2]);
            ExitCode::from(1)
        }
    }
}
RS

  cat > "$HELPER_DIR/src/bin/heif-decode.rs" <<'RS'
use heic_decoder::DecodeGuardrails;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OrientationMode {
    Auto,
    Preserve,
}

fn usage(program: &str) {
    eprintln!(
        "Usage: {program} [--orientation <auto|preserve>] [--max-input-bytes <bytes>] [--max-pixels <pixels>] [--max-temp-spool-bytes <bytes>] [--temp-spool-directory <path>] <input.heif|.heic|.avif> <output.png>"
    );
}

fn parse_u64(flag: &str, value: String) -> Result<u64, String> {
    value
        .parse::<u64>()
        .map_err(|_| format!("{flag} expects a u64 value, got '{value}'"))
}

fn main() -> ExitCode {
    let mut args = std::env::args();
    let program = args.next().unwrap_or_else(|| "heif-decode".to_string());
    let mut positional = Vec::new();
    let mut guardrails = DecodeGuardrails::default();
    let mut orientation = OrientationMode::Auto;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                usage(&program);
                return ExitCode::SUCCESS;
            }
            "--orientation" => {
                let Some(value) = args.next() else {
                    eprintln!("missing value for --orientation");
                    usage(&program);
                    return ExitCode::from(2);
                };
                orientation = match value.as_str() {
                    "auto" => OrientationMode::Auto,
                    "preserve" => OrientationMode::Preserve,
                    _ => {
                        eprintln!("--orientation expects auto or preserve, got '{value}'");
                        usage(&program);
                        return ExitCode::from(2);
                    }
                };
            }
            "--max-input-bytes" => {
                let Some(value) = args.next() else {
                    eprintln!("missing value for --max-input-bytes");
                    usage(&program);
                    return ExitCode::from(2);
                };
                guardrails.max_input_bytes = match parse_u64("--max-input-bytes", value) {
                    Ok(value) => Some(value),
                    Err(message) => {
                        eprintln!("{message}");
                        return ExitCode::from(2);
                    }
                };
            }
            "--max-pixels" => {
                let Some(value) = args.next() else {
                    eprintln!("missing value for --max-pixels");
                    usage(&program);
                    return ExitCode::from(2);
                };
                guardrails.max_pixels = match parse_u64("--max-pixels", value) {
                    Ok(value) => Some(value),
                    Err(message) => {
                        eprintln!("{message}");
                        return ExitCode::from(2);
                    }
                };
            }
            "--max-temp-spool-bytes" => {
                let Some(value) = args.next() else {
                    eprintln!("missing value for --max-temp-spool-bytes");
                    usage(&program);
                    return ExitCode::from(2);
                };
                guardrails.max_temp_spool_bytes =
                    match parse_u64("--max-temp-spool-bytes", value) {
                        Ok(value) => Some(value),
                        Err(message) => {
                            eprintln!("{message}");
                            return ExitCode::from(2);
                        }
                    };
            }
            "--temp-spool-directory" => {
                let Some(value) = args.next() else {
                    eprintln!("missing value for --temp-spool-directory");
                    usage(&program);
                    return ExitCode::from(2);
                };
                guardrails.temp_spool_directory = Some(PathBuf::from(value));
            }
            _ if arg.starts_with('-') => {
                eprintln!("unknown option '{arg}'");
                usage(&program);
                return ExitCode::from(2);
            }
            _ => positional.push(arg),
        }
    }

    if positional.len() != 2 {
        eprintln!("expected <input> and <output>");
        usage(&program);
        return ExitCode::from(2);
    }

    let input = Path::new(&positional[0]);
    let output = Path::new(&positional[1]);
    let mut decoded = match heic_decoder::decode_path_to_rgba_with_guardrails(input, guardrails) {
        Ok(decoded) => decoded,
        Err(error) => {
            eprintln!(
                "Decode failed [category={}]: {error}",
                error.category().as_str()
            );
            return ExitCode::from(1);
        }
    };

    if orientation == OrientationMode::Auto && heic_decoder::path_extension_is_heif(input) {
        if let Ok(hint) = heic_decoder::exif_orientation_hint_from_path(input)
            && let Some(orientation) = hint.orientation_to_apply()
        {
            match decoded.apply_exif_orientation(orientation) {
                Ok(oriented) => decoded = oriented,
                Err(error) => {
                    eprintln!(
                        "Decode failed [category={}]: {error}",
                        error.category().as_str()
                    );
                    return ExitCode::from(1);
                }
            }
        }
    }

    match heic_decoder::write_decoded_rgba_to_png(&decoded, output) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!(
                "Decode failed [category={}]: {error}",
                error.category().as_str()
            );
            ExitCode::from(1)
        }
    }
}
RS

  cat > "$HELPER_DIR/src/bin/heif-ingestion-bench.rs" <<'RS'
use heic_decoder::{DecodedRgbaImage, DecodedRgbaPixels, decode_bytes_to_rgba, decode_path_to_rgba};
use std::error::Error;
use std::fs;
use std::path::Path;

fn checksum(samples: &[u8]) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    ((samples[0] as u64) << 16)
        ^ ((samples[samples.len() / 2] as u64) << 8)
        ^ samples[samples.len() - 1] as u64
        ^ samples.len() as u64
}

fn checksum_u16(samples: &[u16]) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    ((samples[0] as u64) << 32)
        ^ ((samples[samples.len() / 2] as u64) << 16)
        ^ samples[samples.len() - 1] as u64
        ^ samples.len() as u64
}

fn image_checksum(image: &DecodedRgbaImage) -> u64 {
    let pixels = match &image.pixels {
        DecodedRgbaPixels::U8(samples) => checksum(samples),
        DecodedRgbaPixels::U16(samples) => checksum_u16(samples),
    };
    ((image.width as u64) << 32) ^ image.height as u64 ^ pixels
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        return Err("Usage: heif-ingestion-bench <path|bytes> <input.heic|.heif|.avif>".into());
    }
    let input = Path::new(&args[2]);
    let decoded = match args[1].as_str() {
        "path" => decode_path_to_rgba(input)?,
        "bytes" => {
            let bytes = fs::read(input)?;
            decode_bytes_to_rgba(&bytes)?
        }
        other => return Err(format!("unsupported mode '{other}'").into()),
    };
    println!("{}", image_checksum(&decoded));
    Ok(())
}
RS

  cat > "$HELPER_DIR/src/bin/heif-image-adapter-bench.rs" <<'RS'
use heic_decoder::image_integration::register_image_decoder_hooks_with_guardrails;
use heic_decoder::{DecodeGuardrails, DecodedRgbaImage, DecodedRgbaPixels, decode_path_to_rgba};
use image::{DynamicImage, ImageDecoder, ImageReader, Limits};
use std::error::Error;
use std::path::Path;

fn checksum(samples: &[u8]) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    ((samples[0] as u64) << 16)
        ^ ((samples[samples.len() / 2] as u64) << 8)
        ^ samples[samples.len() - 1] as u64
        ^ samples.len() as u64
}

fn checksum_u16(samples: &[u16]) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    ((samples[0] as u64) << 32)
        ^ ((samples[samples.len() / 2] as u64) << 16)
        ^ samples[samples.len() - 1] as u64
        ^ samples.len() as u64
}

fn direct_checksum(image: &DecodedRgbaImage) -> u64 {
    let pixels = match &image.pixels {
        DecodedRgbaPixels::U8(samples) => checksum(samples),
        DecodedRgbaPixels::U16(samples) => checksum_u16(samples),
    };
    ((image.width as u64) << 32) ^ image.height as u64 ^ pixels
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        return Err("Usage: heif-image-adapter-bench <direct|adapter> <input.heic|.heif|.avif>".into());
    }
    let input = Path::new(&args[2]);
    let value = match args[1].as_str() {
        "direct" => direct_checksum(&decode_path_to_rgba(input)?),
        "adapter" => {
            let _ = register_image_decoder_hooks_with_guardrails(DecodeGuardrails {
                max_input_bytes: Some(128 * 1024 * 1024),
                max_pixels: Some(256_000_000),
                max_temp_spool_bytes: Some(256 * 1024 * 1024),
                temp_spool_directory: None,
            });
            let mut decoder = ImageReader::open(input)?.with_guessed_format()?.into_decoder()?;
            let _icc_profile = decoder.icc_profile()?;
            let mut limits = Limits::default();
            limits.reserve(decoder.total_bytes())?;
            decoder.set_limits(limits)?;
            let decoded = DynamicImage::from_decoder(decoder)?;
            let (width, height) = (decoded.width(), decoded.height());
            let pixels = match decoded {
                DynamicImage::ImageRgba8(buffer) => checksum(buffer.as_raw()),
                DynamicImage::ImageRgba16(buffer) => checksum_u16(buffer.as_raw()),
                other => return Err(format!("unsupported adapter output {:?}", other.color()).into()),
            };
            ((width as u64) << 32) ^ height as u64 ^ pixels
        }
        other => return Err(format!("unsupported mode '{other}'").into()),
    };
    println!("{value}");
    Ok(())
}
RS

  cat > "$HELPER_DIR/src/bin/heif-image-hook-check.rs" <<'RS'
use heic_decoder::image_integration::register_image_decoder_hooks_with_guardrails;
use heic_decoder::{DecodeGuardrails, DecodedRgbaPixels, decode_path_to_rgba};
use image::{DynamicImage, ImageDecoder, ImageReader, Limits};
use std::error::Error;
use std::path::Path;

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 {
        return Err("Usage: heif-image-hook-check <input.heic|.heif|.avif>".into());
    }

    let input = Path::new(&args[1]);
    let direct = decode_path_to_rgba(input)?;

    let _ = register_image_decoder_hooks_with_guardrails(DecodeGuardrails {
        max_input_bytes: Some(128 * 1024 * 1024),
        max_pixels: Some(256_000_000),
        max_temp_spool_bytes: Some(256 * 1024 * 1024),
        temp_spool_directory: None,
    });
    let mut decoder = ImageReader::open(input)?.with_guessed_format()?.into_decoder()?;
    let icc_profile = decoder.icc_profile()?;
    let mut limits = Limits::default();
    limits.reserve(decoder.total_bytes())?;
    decoder.set_limits(limits)?;
    if icc_profile != direct.icc_profile {
        return Err("image hook ICC profile differs from direct decode".into());
    }

    let decoded = DynamicImage::from_decoder(decoder)?;
    if decoded.width() != direct.width || decoded.height() != direct.height {
        return Err(format!(
            "image hook dimensions {}x{} differ from direct decode {}x{}",
            decoded.width(),
            decoded.height(),
            direct.width,
            direct.height
        )
        .into());
    }

    match (&direct.pixels, decoded) {
        (DecodedRgbaPixels::U8(expected), DynamicImage::ImageRgba8(actual))
            if expected == actual.as_raw() => {}
        (DecodedRgbaPixels::U16(expected), DynamicImage::ImageRgba16(actual))
            if expected == actual.as_raw() => {}
        (DecodedRgbaPixels::U8(expected), DynamicImage::ImageRgba8(actual)) => {
            return Err(format!(
                "image hook RGBA8 pixel mismatch: direct_samples={} hook_samples={}",
                expected.len(),
                actual.as_raw().len()
            )
            .into());
        }
        (DecodedRgbaPixels::U16(expected), DynamicImage::ImageRgba16(actual)) => {
            return Err(format!(
                "image hook RGBA16 pixel mismatch: direct_samples={} hook_samples={}",
                expected.len(),
                actual.as_raw().len()
            )
            .into());
        }
        (DecodedRgbaPixels::U8(_), other) => {
            return Err(format!("image hook color mismatch: expected RGBA8, got {:?}", other.color()).into());
        }
        (DecodedRgbaPixels::U16(_), other) => {
            return Err(format!("image hook color mismatch: expected RGBA16, got {:?}", other.color()).into());
        }
    }

    Ok(())
}
RS

  cat > "$HELPER_DIR/src/bin/heif-stream-concurrency-bench.rs" <<'RS'
use heic_decoder::{DecodedRgbaImage, DecodedRgbaPixels, decode_path_to_rgba, decode_read_to_rgba};
use std::error::Error;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

#[derive(Clone, Copy, Debug)]
enum Mode {
    Path,
    Read,
}

fn checksum(samples: &[u8]) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    ((samples[0] as u64) << 16)
        ^ ((samples[samples.len() / 2] as u64) << 8)
        ^ samples[samples.len() - 1] as u64
        ^ samples.len() as u64
}

fn checksum_u16(samples: &[u16]) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    ((samples[0] as u64) << 32)
        ^ ((samples[samples.len() / 2] as u64) << 16)
        ^ samples[samples.len() - 1] as u64
        ^ samples.len() as u64
}

fn image_checksum(image: &DecodedRgbaImage) -> u64 {
    let pixels = match &image.pixels {
        DecodedRgbaPixels::U8(samples) => checksum(samples),
        DecodedRgbaPixels::U16(samples) => checksum_u16(samples),
    };
    ((image.width as u64) << 32) ^ image.height as u64 ^ pixels
}

fn decode_checksum(mode: Mode, input: &Path) -> Result<u64, Box<dyn Error + Send + Sync>> {
    let decoded = match mode {
        Mode::Path => decode_path_to_rgba(input)?,
        Mode::Read => decode_read_to_rgba(File::open(input)?)?,
    };
    Ok(image_checksum(&decoded))
}

fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 5 {
        return Err("Usage: heif-stream-concurrency-bench <path|read> <workers> <iterations-per-worker> <input.heic|.avif> [more inputs...]".into());
    }
    let mode = match args[1].as_str() {
        "path" => Mode::Path,
        "read" => Mode::Read,
        other => return Err(format!("unsupported mode '{other}'").into()),
    };
    let workers = args[2].parse::<usize>()?;
    let iterations = args[3].parse::<usize>()?;
    if workers == 0 || iterations == 0 {
        return Err("workers and iterations must be greater than zero".into());
    }
    let inputs = Arc::new(args[4..].iter().map(PathBuf::from).collect::<Vec<_>>());
    let expected = Arc::new(
        inputs
            .iter()
            .map(|input| decode_checksum(Mode::Path, input))
            .collect::<Result<Vec<_>, _>>()?,
    );

    let mut handles = Vec::with_capacity(workers);
    for worker_id in 0..workers {
        let inputs = Arc::clone(&inputs);
        let expected = Arc::clone(&expected);
        handles.push(thread::spawn(move || {
            let mut aggregate = 0_u64;
            for iteration in 0..iterations {
                let index = (worker_id + iteration) % inputs.len();
                let actual = decode_checksum(mode, &inputs[index])?;
                if actual != expected[index] {
                    return Err(format!(
                        "checksum mismatch for {}: expected={} actual={}",
                        inputs[index].display(),
                        expected[index],
                        actual
                    )
                    .into());
                }
                aggregate ^= actual.rotate_left(((worker_id + iteration) % 63 + 1) as u32);
            }
            Ok::<u64, Box<dyn Error + Send + Sync>>(aggregate)
        }));
    }

    let mut aggregate = 0_u64;
    for handle in handles {
        aggregate ^= handle.join().map_err(|_| "worker panicked")??;
    }
    println!("ops={} checksum={aggregate}", workers * iterations);
    Ok(())
}
RS
}

build_helper() {
  require_cmd cargo
  ensure_helper_sources
  log helper "Building generated helper binaries at $HELPER_DIR"
  cargo build --manifest-path "$HELPER_DIR/Cargo.toml" --release --bins
}

libheif_oracle_ready() {
  local listing
  listing="$("$LIBHEIF_DEC_BIN" --list-decoders 2>/dev/null || true)"
  [[ -n "$listing" ]] || return 1

  has_decoder_in_section() {
    local section="$1"
    awk -v section="$section" '
      $0 == section ":" { in_section=1; next }
      in_section && $0 ~ /^[^[:space:]].*:$/ { in_section=0 }
      in_section && $0 ~ /^- / { has_decoder=1 }
      END { exit(has_decoder ? 0 : 1) }
    ' <<<"$listing"
  }

  has_decoder_in_section "AVIF decoders" &&
    has_decoder_in_section "HEIC decoders" &&
    has_decoder_in_section "JPEG decoders" &&
    has_decoder_in_section "JPEG 2000 decoders" &&
    has_decoder_in_section "uncompressed"
}

build_libheif_decoder() {
  resolve_libheif_paths

  local source_available=0
  is_libheif_source_dir "$LIBHEIF_SOURCE_DIR" && source_available=1

  local rebuild_reason=""
  if [[ "$LIBHEIF_RECONFIGURE" == "1" ]]; then
    rebuild_reason="forced by LIBHEIF_RECONFIGURE=1"
  elif [[ ! -x "$LIBHEIF_DEC_BIN" ]]; then
    rebuild_reason="decoder binary missing"
  elif [[ "$LIBHEIF_REQUIRE_ORACLE_DECODERS" == "1" ]] && ! libheif_oracle_ready; then
    rebuild_reason="existing build missing required validator decoders"
  fi

  if [[ -z "$rebuild_reason" ]]; then
    return
  fi

  if [[ "$source_available" -eq 0 ]]; then
    fail "Could not find a libheif validator checkout. Set HEIC_LIBHEIF_SOURCE_DIR, clone/symlink it into .heic-test-assets/libheif, or clone it directly into .heic-test-assets. The source checkout is needed to build heif-dec and to provide the default corpus."
  fi

  if [[ -z "$LIBHEIF_DEC_BIN_EXPLICIT" ]]; then
    LIBHEIF_DEC_BIN="$LIBHEIF_BUILD_DIR/examples/heif-dec"
  fi

  require_cmd cmake

  local cmake_args=(
    "-DCMAKE_BUILD_TYPE=Release"
    "-DBUILD_TESTING=OFF"
    "-DWITH_EXAMPLES=ON"
    "-DENABLE_PLUGIN_LOADING=OFF"
    "-DWITH_LIBDE265=ON"
    "-DWITH_AOM_DECODER=ON"
    "-DWITH_DAV1D=ON"
    "-DWITH_UNCOMPRESSED_CODEC=ON"
    "-DWITH_OpenJPEG_DECODER=ON"
    "-DWITH_JPEG_DECODER=ON"
    "-DWITH_HEADER_COMPRESSION=ON"
  )

  if [[ -n "${LIBHEIF_CMAKE_ARGS:-}" ]]; then
    # shellcheck disable=SC2206
    local extra_cmake_args=( ${LIBHEIF_CMAKE_ARGS} )
    cmake_args+=("${extra_cmake_args[@]}")
  fi

  log validator "Building validator at $LIBHEIF_BUILD_DIR ($rebuild_reason)"
  cmake -S "$LIBHEIF_SOURCE_DIR" -B "$LIBHEIF_BUILD_DIR" "${cmake_args[@]}" >/dev/null
  cmake --build "$LIBHEIF_BUILD_DIR" --target heif-dec heif-info --parallel >/dev/null

  [[ -x "$LIBHEIF_DEC_BIN" ]] || fail "Could not build heif-dec at $LIBHEIF_DEC_BIN"
  if [[ "$LIBHEIF_REQUIRE_ORACLE_DECODERS" == "1" ]] && ! libheif_oracle_ready; then
    fail "Built validator is missing required decoders."
  fi
}

ente_fixtures_files_dir() {
  printf '%s\n' "$ENTE_FIXTURES_DIR/$ENTE_FIXTURES_SUBDIR"
}

is_ente_fixtures_files_dir() {
  local dir="$1"
  [[ -d "$dir" ]] || return 1
  find "$dir" -type f \( -iname '*.heif' -o -iname '*.heic' -o -iname '*.avif' \) -print -quit 2>/dev/null | grep -q .
}

# The ente test-fixtures HEIC corpus is part of the default corpus. It is
# fetched on demand into the gitignored asset directory (sparse clone of only
# the HEIC fixture subtree) and never checked into this repository. Best
# effort: if it is missing and cannot be fetched, a warning is printed and the
# default corpus falls back to the libheif dirs only.
ensure_ente_fixtures() {
  local files_dir
  files_dir="$(ente_fixtures_files_dir)"
  if is_ente_fixtures_files_dir "$files_dir"; then
    return 0
  fi

  if [[ -n "$ENTE_FIXTURES_EXPLICIT" || -e "$ENTE_FIXTURES_DIR" ]]; then
    log fixtures "WARNING: no HEIC fixture files under $files_dir; continuing without the ente fixtures corpus" >&2
    return 1
  fi

  log fixtures "Fetching ente test-fixtures HEIC corpus into $ENTE_FIXTURES_DIR" >&2
  if ! command -v git >/dev/null 2>&1 \
    || ! git clone --quiet --depth 1 --filter=blob:none --no-checkout \
          "$ENTE_FIXTURES_REPO_URL" "$ENTE_FIXTURES_DIR" >&2 \
    || ! git -C "$ENTE_FIXTURES_DIR" sparse-checkout set "$ENTE_FIXTURES_SUBDIR" >&2 \
    || ! git -C "$ENTE_FIXTURES_DIR" checkout --quiet >&2 \
    || ! is_ente_fixtures_files_dir "$files_dir"; then
    rm -rf "$ENTE_FIXTURES_DIR"
    log fixtures "WARNING: could not fetch $ENTE_FIXTURES_REPO_URL; continuing without the ente fixtures corpus" >&2
    return 1
  fi
}

default_corpus_dirs() {
  resolve_libheif_paths

  if ! is_libheif_source_dir "$LIBHEIF_SOURCE_DIR"; then
    fail "No --corpus-dir provided and validator source not found. Set HEIC_LIBHEIF_SOURCE_DIR, clone/symlink it into .heic-test-assets/libheif, or clone it directly into .heic-test-assets."
  fi
  printf '%s\n' \
    "$LIBHEIF_SOURCE_DIR/examples" \
    "$LIBHEIF_SOURCE_DIR/tests/data" \
    "$LIBHEIF_SOURCE_DIR/fuzzing/data/corpus"
  if ensure_ente_fixtures; then
    ente_fixtures_files_dir
  fi
  # Locally generated HEVC stress corpus (scripts/gen_stress_corpus.sh or
  # `scripts/heic_tests.sh gen-stress`); included when present.
  if [[ -d "$ASSET_ROOT/stress-corpus" ]]; then
    printf '%s\n' "$ASSET_ROOT/stress-corpus"
  fi
}

gather_files() {
  local dir
  for dir in "$@"; do
    if [[ -d "$dir" ]]; then
      find "$dir" -type f \( -iname '*.heif' -o -iname '*.heic' -o -iname '*.avif' \)
    fi
  done | sort -u
}

display_path() {
  local path="$1"
  case "$path" in
    "$ROOT_DIR"/*) echo "${path#$ROOT_DIR/}" ;;
    "$LIBHEIF_SOURCE_DIR"/*) echo "libheif/${path#$LIBHEIF_SOURCE_DIR/}" ;;
    *) echo "$path" ;;
  esac
}

file_id() {
  printf '%s' "$1" | shasum -a 256 | awk '{print $1}'
}

clean_output_variants() {
  local requested="$1"
  local dir filename stem ext
  dir="${requested%/*}"
  filename="${requested##*/}"
  [[ "$dir" == "$requested" ]] && dir="."
  stem="${filename%.*}"
  ext="${filename##*.}"
  rm -f "$requested"
  find "$dir" -maxdepth 1 -type f -name "${stem}-*.${ext}" -delete
}

resolve_output_file() {
  local requested="$1"
  local dir filename stem ext candidate
  if [[ -f "$requested" ]]; then
    echo "$requested"
    return 0
  fi
  dir="${requested%/*}"
  filename="${requested##*/}"
  [[ "$dir" == "$requested" ]] && dir="."
  stem="${filename%.*}"
  ext="${filename##*.}"
  candidate="$(find "$dir" -maxdepth 1 -type f -name "${stem}-*.${ext}" | sort | head -n 1)"
  [[ -n "$candidate" ]] || return 1
  echo "$candidate"
}

decode_with_helper() {
  "$HELPER_BIN_DIR/heif-decode" --orientation preserve "$1" "$2"
}

check_with_image_hook_helper() {
  "$HELPER_BIN_DIR/heif-image-hook-check" "$1"
}

failure_reason_from_log() {
  tr '\n' ' ' < "$1" | sed 's/[[:space:]][[:space:]]*/ /g; s/^ //; s/ $//'
}

decode_failure_category_from_log() {
  sed -nE 's/^Decode failed \[category=([^]]+)\]:.*/\1/p' "$1" | head -n 1
}

reason_contains() {
  local haystack="$1"
  local needle="$2"
  case "$haystack" in
    *"$needle"*) return 0 ;;
    *) return 1 ;;
  esac
}

libheif_relative_path() {
  local path="$1"
  case "$path" in
    "$LIBHEIF_SOURCE_DIR"/*) printf '%s\n' "${path#$LIBHEIF_SOURCE_DIR/}" ;;
    "$ASSET_ROOT/libheif"/*) printf '%s\n' "${path#$ASSET_ROOT/libheif/}" ;;
    .heic-test-assets/libheif/*) printf '%s\n' "${path#.heic-test-assets/libheif/}" ;;
    libheif/*) printf '%s\n' "${path#libheif/}" ;;
    *) return 1 ;;
  esac
}

expected_validator_failure_kind() {
  local input_file="$1"
  local validator_reason="$2"
  local rel_path
  rel_path="$(libheif_relative_path "$input_file")" || return 1

  case "$rel_path" in
    fuzzing/data/corpus/avc32.heif|\
    fuzzing/data/corpus/vvc32.heif)
      reason_contains "$validator_reason" "Support for this compression format" || return 1
      echo "missing-validator-codec"
      return 0
      ;;
    fuzzing/data/corpus/avif32-mini.heif|\
    fuzzing/data/corpus/hevc32-mini.heif|\
    tests/data/lightning_mini.heif|\
    tests/data/simple_osm_tile_alpha.avif|\
    tests/data/simple_osm_tile_meta.avif)
      reason_contains "$validator_reason" "No supported brands found" || return 1
      echo "unsupported-brand"
      return 0
      ;;
    fuzzing/data/corpus/clap-overflow-divide-zero.heic|\
    fuzzing/data/corpus/clusterfuzz-testcase-minimized-file-fuzzer-4616081830051840.heic|\
    fuzzing/data/corpus/clusterfuzz-testcase-minimized-file-fuzzer-5120279175102464.heic|\
    fuzzing/data/corpus/clusterfuzz-testcase-minimized-file-fuzzer-5643900194127872.heic|\
    fuzzing/data/corpus/clusterfuzz-testcase-minimized-file-fuzzer-5651556035198976.heic|\
    fuzzing/data/corpus/clusterfuzz-testcase-minimized-file-fuzzer-5662360964956160.heic|\
    fuzzing/data/corpus/clusterfuzz-testcase-minimized-file-fuzzer-5686319331672064.heic|\
    fuzzing/data/corpus/clusterfuzz-testcase-minimized-file-fuzzer-5718632116518912.heic|\
    fuzzing/data/corpus/clusterfuzz-testcase-minimized-file-fuzzer-5720856641142784.heic|\
    fuzzing/data/corpus/clusterfuzz-testcase-minimized-file-fuzzer-5724458239655936.heic|\
    fuzzing/data/corpus/clusterfuzz-testcase-minimized-file-fuzzer-5732616832024576.heic|\
    fuzzing/data/corpus/clusterfuzz-testcase-minimized-file-fuzzer-5757633845264384.heic|\
    fuzzing/data/corpus/crash-20ca2625096a205937b809a7841e7f019f0b2dc6.heic|\
    fuzzing/data/corpus/github_138.heic|\
    fuzzing/data/corpus/github_367_2.heic|\
    fuzzing/data/corpus/github_44.heic|\
    fuzzing/data/corpus/github_45.heic|\
    fuzzing/data/corpus/github_48.heic|\
    fuzzing/data/corpus/github_50.heic|\
    fuzzing/data/corpus/j2k-siz-segment-undersized.heic|\
    fuzzing/data/corpus/region-mask-missing-refs.heic)
      reason_contains "$validator_reason" "Unsupported data version" || return 1
      echo "unsupported-box-version"
      return 0
      ;;
    fuzzing/data/corpus/github_15.heic|\
    fuzzing/data/corpus/github_20.heic|\
    fuzzing/data/corpus/github_46_2.heic|\
    fuzzing/data/corpus/github_47.heic|\
    fuzzing/data/corpus/github_49.heic)
      reason_contains "$validator_reason" "valid box length" || return 1
      echo "invalid-or-truncated-box"
      return 0
      ;;
    fuzzing/data/corpus/github_46_1.heic)
      reason_contains "$validator_reason" "Invalid box size" || return 1
      echo "invalid-box-size"
      return 0
      ;;
    fuzzing/data/corpus/clusterfuzz-testcase-minimized-file-fuzzer-6045213633282048.heic|\
    fuzzing/data/corpus/rgb_generic_compressed_brotli.heic|\
    fuzzing/data/corpus/rgb_generic_compressed_defl.heic|\
    fuzzing/data/corpus/rgb_generic_compressed_zlib.heic)
      reason_contains "$validator_reason" "Unsupported essential item property" || return 1
      echo "unsupported-essential-property"
      return 0
      ;;
    fuzzing/data/corpus/rgb_generic_compressed_tile_deflate.heic|\
    fuzzing/data/corpus/rgb_generic_compressed_zlib_rows.heic|\
    fuzzing/data/corpus/rgb_generic_compressed_zlib_tiled.heic)
      reason_contains "$validator_reason" "Unsupported cmpC compressed unit type" || return 1
      echo "unsupported-compressed-unit"
      return 0
      ;;
    fuzzing/data/corpus/uncompressed_comp_Y16U16V16_422.heic|\
    fuzzing/data/corpus/uncompressed_mix_Y16U16V16_422.heic|\
    tests/data/uncompressed_comp_Y16U16V16_422.heif|\
    tests/data/uncompressed_mix_Y16U16V16_422.heif)
      reason_contains "$validator_reason" "Unsupported color conversion" || return 1
      echo "unsupported-validator-color-conversion"
      return 0
      ;;
    fuzzing/data/corpus/clusterfuzz-testcase-minimized-file-fuzzer-5671864958976000.heic|\
    fuzzing/data/corpus/clusterfuzz-testcase-minimized-file-fuzzer-5752063708495872.heic)
      reason_contains "$validator_reason" "Decoder plugin generated an error" || return 1
      echo "validator-decoder-error"
      return 0
      ;;
    fuzzing/data/corpus/hbo_heif_context.h_126_1.heic|\
    fuzzing/data/corpus/uaf_heif_context.h_117_1.heic)
      reason_contains "$validator_reason" "Non-existing item ID referenced" || return 1
      echo "broken-item-reference"
      return 0
      ;;
    fuzzing/data/corpus/github_367_1.heic)
      reason_contains "$validator_reason" "Security limit exceeded" || return 1
      echo "validator-security-limit"
      return 0
      ;;
    *) return 1 ;;
  esac
}

expected_rust_failure_kind() {
  local input_file="$1"
  local rust_category="$2"
  local rust_reason="$3"
  local rel_path
  rel_path="$(libheif_relative_path "$input_file")" || return 1

  case "$rel_path" in
    fuzzing/data/corpus/j2k32.heif)
      [[ "$rust_category" == "parse" ]] || return 1
      reason_contains "$rust_reason" "item_type j2k1" || return 1
      echo "unsupported-jpeg2000-codec"
      return 0
      ;;
    fuzzing/data/corpus/jpeg32.heif)
      [[ "$rust_category" == "parse" ]] || return 1
      reason_contains "$rust_reason" "item_type jpeg" || return 1
      echo "unsupported-jpeg-codec"
      return 0
      ;;
    fuzzing/data/corpus/unci32.heif)
      [[ "$rust_category" == "unsupported-feature" ]] || return 1
      reason_contains "$rust_reason" "uncC block/endian flags" || return 1
      echo "unsupported-uncompressed-variant"
      return 0
      ;;
    fuzzing/data/corpus/avc32.heif)
      [[ "$rust_category" == "parse" ]] || return 1
      reason_contains "$rust_reason" "item_type avc1" || return 1
      echo "unsupported-avc-codec"
      return 0
      ;;
    fuzzing/data/corpus/vvc32.heif)
      [[ "$rust_category" == "parse" ]] || return 1
      reason_contains "$rust_reason" "item_type vvc1" || return 1
      echo "unsupported-vvc-codec"
      return 0
      ;;
    *) return 1 ;;
  esac
}

image_dim() {
  ffprobe -v error -select_streams v:0 -show_entries stream=width,height -of csv=p=0:s=x "$1"
}

png_to_rgba() {
  ffmpeg -v error -y -i "$1" -map 0:v:0 -f rawvideo -pix_fmt rgba "$2"
}

# Colour-profile oracle check: heif-dec embeds the container's raw ICC
# profile in its PNG output, so whenever the validator PNG carries a profile
# the Rust PNG must carry byte-identical profile data. A Rust-only profile
# is allowed and expected: the Rust decoder synthesizes an ICC profile from
# nclx colour information, which heif-dec does not embed.
# Prints a failure description and returns 1 on mismatch.
compare_png_icc() {
  local ref_png="$1" rust_png="$2" prefix="$3"
  local ref_label="${4:-validator}" rust_label="${5:-rust}"
  local ref_icc="$prefix.ref.icc" rust_icc="$prefix.rust.icc"
  local ref_status=0 rust_status=0
  "$HELPER_BIN_DIR/heif-png-icc" "$ref_png" "$ref_icc" 2>/dev/null || ref_status=$?
  "$HELPER_BIN_DIR/heif-png-icc" "$rust_png" "$rust_icc" 2>/dev/null || rust_status=$?

  if [[ "$ref_status" -ne 0 && "$ref_status" -ne 3 ]]; then
    echo "could not read validator PNG ICC profile (status=$ref_status)"
    return 1
  fi
  if [[ "$rust_status" -ne 0 && "$rust_status" -ne 3 ]]; then
    echo "could not read rust PNG ICC profile (status=$rust_status)"
    return 1
  fi
  if [[ "$ref_status" -eq 3 ]]; then
    return 0
  fi
  if [[ "$rust_status" -eq 3 ]]; then
    echo "icc profile missing: $ref_label PNG embeds one, $rust_label PNG has none"
    return 1
  fi
  if ! cmp -s "$ref_icc" "$rust_icc"; then
    local ref_hash rust_hash
    ref_hash="$(shasum -a 256 "$ref_icc" | awk '{print $1}')"
    rust_hash="$(shasum -a 256 "$rust_icc" | awk '{print $1}')"
    echo "icc profile mismatch $ref_label=$ref_hash $rust_label=$rust_hash"
    return 1
  fi
  return 0
}

float_add() {
  awk -v a="$1" -v b="$2" 'BEGIN { printf "%.8f", a + b }'
}

float_mul() {
  awk -v a="$1" -v b="$2" 'BEGIN { printf "%.8f", a * b }'
}

float_div() {
  awk -v a="$1" -v b="$2" 'BEGIN { if (b == 0) { print "inf" } else { printf "%.8f", a / b } }'
}

float_floor_time() {
  awk -v value="$1" 'BEGIN { if (value <= 0) printf "%.8f", 0.000001; else printf "%.8f", value }'
}

float_leq() {
  awk -v a="$1" -v b="$2" 'BEGIN { if (a <= b) print 1; else print 0 }'
}

float_gt() {
  awk -v a="$1" -v b="$2" 'BEGIN { if (a > b) print 1; else print 0 }'
}

bytes_to_mib() {
  awk -v bytes="$1" 'BEGIN { printf "%.8f", bytes / 1048576 }'
}

file_size() {
  stat -f '%z' "$1" 2>/dev/null || stat -c '%s' "$1" 2>/dev/null || echo 0
}

LOADED_FILES=()

load_corpus() {
  local mode="$1"
  local quick_limit="$2"
  shift 2
  local corpus_dirs=("$@")

  if [[ ${#corpus_dirs[@]} -eq 0 ]]; then
    corpus_dirs=()
    while IFS= read -r line; do
      corpus_dirs+=("$line")
    done < <(default_corpus_dirs)
  fi

  LOADED_FILES=()
  while IFS= read -r line; do
    LOADED_FILES+=("$line")
  done < <(gather_files "${corpus_dirs[@]}")
  [[ ${#LOADED_FILES[@]} -gt 0 ]] || fail "No input files found in corpus dirs: ${corpus_dirs[*]}"

  if [[ "$mode" == "quick" && "${#LOADED_FILES[@]}" -gt "$quick_limit" ]]; then
    local selected=()
    local total_files="${#LOADED_FILES[@]}"
    local step=$((total_files / quick_limit))
    [[ "$step" -lt 1 ]] && step=1
    local idx=0
    while [[ "$idx" -lt "$total_files" && "${#selected[@]}" -lt "$quick_limit" ]]; do
      selected+=("${LOADED_FILES[$idx]}")
      idx=$((idx + step))
    done
    idx=0
    while [[ "$idx" -lt "$total_files" && "${#selected[@]}" -lt "$quick_limit" ]]; do
      selected+=("${LOADED_FILES[$idx]}")
      idx=$((idx + 1))
    done
    LOADED_FILES=("${selected[@]}")
  fi
}

cmd_verify() {
  local mode="quick"
  local quick_limit="${QUICK_LIMIT:-60}"
  local keep_artifacts=0
  local require_exts="${REQUIRE_EXTS:-}"
  local corpus_dirs=()

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --quick) mode="quick"; shift ;;
      --full) mode="full"; shift ;;
      --quick-limit) quick_limit="$2"; shift 2 ;;
      --corpus-dir) corpus_dirs+=("$2"); shift 2 ;;
      --keep-artifacts) keep_artifacts=1; shift ;;
      --require-exts) require_exts="$2"; shift 2 ;;
      -h|--help)
        cat <<'EOF'
Usage: scripts/heic_tests.sh verify [--quick|--full] [--quick-limit n]
       [--corpus-dir dir ...] [--keep-artifacts] [--require-exts heic,avif]
EOF
        return 0
        ;;
      *) fail "Unknown verify option: $1" ;;
    esac
  done

  require_cmd bash
  require_cmd ffmpeg
  require_cmd ffprobe
  require_cmd find
  require_cmd shasum
  require_cmd sort
  require_cmd awk
  require_cmd head
  require_cmd cmp
  require_cmd sed
  require_cmd tr

  build_libheif_decoder
  build_helper

  if [[ ${#corpus_dirs[@]} -gt 0 ]]; then
    load_corpus "$mode" "$quick_limit" "${corpus_dirs[@]}"
  else
    load_corpus "$mode" "$quick_limit"
  fi
  local files=("${LOADED_FILES[@]}")

  local run_dir="$TEST_ROOT/verify/run"
  local ref_dir="$run_dir/ref"
  local rust_dir="$run_dir/rust"
  local tmp_dir="$run_dir/tmp"
  local report_file="$run_dir/report.txt"
  rm -rf "$run_dir"
  mkdir -p "$ref_dir" "$rust_dir" "$tmp_dir"

  echo "mode=$mode files=${#files[@]}" > "$report_file"
  local total=0 skipped=0 passed=0 failed=0
  local image_hook_passed=0
  local expected_validator_failed=0 expected_validator_rust_decoded=0 expected_validator_rust_errors=0
  local expected_rust_failed=0
  local comparable_heif=0 comparable_heic=0 comparable_avif=0
  local failures=()

  local input_file rel_path id ref_png rust_png ref_raw rust_raw ref_actual rust_actual validator_log
  local validator_reason rust_reason ref_dim rust_dim
  for input_file in "${files[@]}"; do
    total=$((total + 1))
    rel_path="$(display_path "$input_file")"
    id="$(file_id "$rel_path")"
    ref_png="$ref_dir/$id.png"
    rust_png="$rust_dir/$id.png"
    ref_raw="$tmp_dir/$id.ref.rgba"
    rust_raw="$tmp_dir/$id.rust.rgba"
    validator_log="$tmp_dir/$id.validator.decode.stderr.log"

    clean_output_variants "$ref_png"
    if "$LIBHEIF_DEC_BIN" --quiet "$input_file" "$ref_png" >/dev/null 2>"$validator_log"; then
      :
    else
      local expected_kind
      validator_reason="$(failure_reason_from_log "$validator_log")"
      if expected_kind="$(expected_validator_failure_kind "$input_file" "$validator_reason")"; then
        local rust_log rust_status rust_category rust_reason
        rust_log="$tmp_dir/$id.rust.decode.stderr.log"
        clean_output_variants "$rust_png"
        if decode_with_helper "$input_file" "$rust_png" >/dev/null 2>"$rust_log"; then
          rust_status=0
        else
          rust_status=$?
        fi

        if [[ "$rust_status" -eq 0 ]]; then
          if rust_actual="$(resolve_output_file "$rust_png")"; then
            expected_validator_failed=$((expected_validator_failed + 1))
            expected_validator_rust_decoded=$((expected_validator_rust_decoded + 1))
            echo "EXPECTED_VALIDATOR_FAIL $rel_path (kind=$expected_kind; rust decoded)" >> "$report_file"
            [[ "$keep_artifacts" -eq 1 ]] || rm -f "$rust_actual"
          else
            failed=$((failed + 1))
            failures+=("$rel_path :: expected validator failure kind=$expected_kind, but Rust exited successfully without output")
            echo "FAIL $rel_path (expected validator failure kind=$expected_kind; rust output file not found)" >> "$report_file"
          fi
        else
          rust_category="$(decode_failure_category_from_log "$rust_log")"
          rust_reason="$(failure_reason_from_log "$rust_log")"
          if [[ "$rust_status" -le 128 && -n "$rust_category" ]]; then
            expected_validator_failed=$((expected_validator_failed + 1))
            expected_validator_rust_errors=$((expected_validator_rust_errors + 1))
            echo "EXPECTED_VALIDATOR_FAIL $rel_path (kind=$expected_kind; rust failed category=$rust_category)" >> "$report_file"
          else
            failed=$((failed + 1))
            failures+=("$rel_path :: expected validator failure kind=$expected_kind, but Rust robustness check failed status=$rust_status category=${rust_category:-none}: $rust_reason")
            echo "FAIL $rel_path (expected validator failure kind=$expected_kind; rust robustness failed status=$rust_status category=${rust_category:-none})" >> "$report_file"
          fi
        fi
      else
        failed=$((failed + 1))
        failures+=("$rel_path :: validator decode failed unexpectedly: ${validator_reason:-no stderr}")
        echo "FAIL $rel_path (unexpected validator decode failure: ${validator_reason:-no stderr})" >> "$report_file"
      fi
      continue
    fi
    if ! ref_actual="$(resolve_output_file "$ref_png")"; then
      failed=$((failed + 1))
      failures+=("$rel_path :: validator output file not found")
      echo "FAIL $rel_path (validator output file not found)" >> "$report_file"
      continue
    fi
    if [[ ! -s "$ref_actual" ]]; then
      failed=$((failed + 1))
      validator_reason="$(failure_reason_from_log "$validator_log")"
      failures+=("$rel_path :: validator produced empty output: ${validator_reason:-no stderr}")
      echo "FAIL $rel_path (validator produced empty output: ${validator_reason:-no stderr})" >> "$report_file"
      continue
    fi
    ref_dim="$(image_dim "$ref_actual" 2>>"$validator_log" || true)"
    if [[ -z "$ref_dim" ]]; then
      failed=$((failed + 1))
      validator_reason="$(failure_reason_from_log "$validator_log")"
      failures+=("$rel_path :: validator produced unreadable output: ${validator_reason:-no stderr}")
      echo "FAIL $rel_path (validator produced unreadable output: ${validator_reason:-no stderr})" >> "$report_file"
      continue
    fi

    case "${input_file##*.}" in
      heif|HEIF) comparable_heif=$((comparable_heif + 1)) ;;
      heic|HEIC) comparable_heic=$((comparable_heic + 1)) ;;
      avif|AVIF) comparable_avif=$((comparable_avif + 1)) ;;
    esac

    local rust_log="$tmp_dir/$id.rust.decode.stderr.log"
    local rust_status
    if decode_with_helper "$input_file" "$rust_png" >/dev/null 2>"$rust_log"; then
      rust_status=0
    else
      rust_status=$?
    fi
    if [[ "$rust_status" -ne 0 ]]; then
      local category rust_reason expected_rust_kind
      category="$(decode_failure_category_from_log "$rust_log")"
      rust_reason="$(failure_reason_from_log "$rust_log")"
      if expected_rust_kind="$(expected_rust_failure_kind "$input_file" "$category" "$rust_reason")"; then
        expected_rust_failed=$((expected_rust_failed + 1))
        echo "EXPECTED_RUST_FAIL $rel_path (kind=$expected_rust_kind; category=$category)" >> "$report_file"
      elif [[ -n "$category" ]]; then
        failed=$((failed + 1))
        failures+=("$rel_path :: rust decoder failed (category=$category)")
        echo "FAIL $rel_path (rust decode failed category=$category)" >> "$report_file"
      else
        failed=$((failed + 1))
        failures+=("$rel_path :: rust decoder failed")
        echo "FAIL $rel_path (rust decode failed)" >> "$report_file"
      fi
      continue
    fi
    if ! rust_actual="$(resolve_output_file "$rust_png")"; then
      failed=$((failed + 1))
      failures+=("$rel_path :: rust output file not found")
      echo "FAIL $rel_path (rust output file not found)" >> "$report_file"
      continue
    fi
    if [[ ! -s "$rust_actual" ]]; then
      failed=$((failed + 1))
      rust_reason="$(failure_reason_from_log "$rust_log")"
      failures+=("$rel_path :: rust decoder produced empty output: ${rust_reason:-no stderr}")
      echo "FAIL $rel_path (rust decoder produced empty output: ${rust_reason:-no stderr})" >> "$report_file"
      continue
    fi

    rust_dim="$(image_dim "$rust_actual" 2>>"$rust_log" || true)"
    if [[ -z "$rust_dim" ]]; then
      failed=$((failed + 1))
      rust_reason="$(failure_reason_from_log "$rust_log")"
      failures+=("$rel_path :: rust decoder produced unreadable output: ${rust_reason:-no stderr}")
      echo "FAIL $rel_path (rust decoder produced unreadable output: ${rust_reason:-no stderr})" >> "$report_file"
      continue
    fi
    if [[ "$ref_dim" != "$rust_dim" ]]; then
      failed=$((failed + 1))
      failures+=("$rel_path :: dimension mismatch ref=$ref_dim rust=$rust_dim")
      echo "FAIL $rel_path (dimension mismatch ref=$ref_dim rust=$rust_dim)" >> "$report_file"
      continue
    fi

    if ! png_to_rgba "$ref_actual" "$ref_raw" >/dev/null 2>&1; then
      failed=$((failed + 1))
      failures+=("$rel_path :: could not convert validator output PNG")
      echo "FAIL $rel_path (validator PNG conversion failed)" >> "$report_file"
      continue
    fi
    if ! png_to_rgba "$rust_actual" "$rust_raw" >/dev/null 2>&1; then
      failed=$((failed + 1))
      failures+=("$rel_path :: could not convert Rust output PNG")
      echo "FAIL $rel_path (rust PNG conversion failed)" >> "$report_file"
      continue
    fi

    if cmp -s "$ref_raw" "$rust_raw"; then
      local icc_failure
      if icc_failure="$(compare_png_icc "$ref_actual" "$rust_actual" "$tmp_dir/$id")"; then
        # Deliberately run for EVERY passing file even though it re-decodes
        # the file twice more (direct + hook): the image-crate hook is a
        # separate decode path (lazy adapter, caller-buffer slices) whose
        # parity with the direct decode is a hard correctness requirement,
        # so it gets the same per-file coverage as the validator comparison.
        # Do not sample or gate this to save CI time.
        local hook_log hook_status
        hook_log="$tmp_dir/$id.image-hook.check.stderr.log"
        if check_with_image_hook_helper "$input_file" >/dev/null 2>"$hook_log"; then
          hook_status=0
        else
          hook_status=$?
        fi

        if [[ "$hook_status" -ne 0 ]]; then
          local hook_reason
          hook_reason="$(failure_reason_from_log "$hook_log")"
          failed=$((failed + 1))
          failures+=("$rel_path :: image hook check failed status=$hook_status: ${hook_reason:-no stderr}")
          echo "FAIL $rel_path (image hook check failed status=$hook_status)" >> "$report_file"
        else
          image_hook_passed=$((image_hook_passed + 1))
          passed=$((passed + 1))
          echo "PASS $rel_path (direct+image-hook)" >> "$report_file"
        fi
      else
        failed=$((failed + 1))
        failures+=("$rel_path :: $icc_failure")
        echo "FAIL $rel_path ($icc_failure)" >> "$report_file"
      fi
    else
      failed=$((failed + 1))
      local ref_hash rust_hash
      ref_hash="$(shasum -a 256 "$ref_raw" | awk '{print $1}')"
      rust_hash="$(shasum -a 256 "$rust_raw" | awk '{print $1}')"
      failures+=("$rel_path :: pixel mismatch ref=$ref_hash rust=$rust_hash")
      echo "FAIL $rel_path (pixel mismatch ref=$ref_hash rust=$rust_hash)" >> "$report_file"
    fi

    if [[ "$keep_artifacts" -eq 0 ]]; then
      rm -f "$ref_raw" "$rust_raw" "$tmp_dir/$id.ref.icc" "$tmp_dir/$id.rust.icc"
    fi
  done

  [[ "$keep_artifacts" -eq 1 ]] || rm -rf "$tmp_dir"
  log verify "Summary: total=$total skipped=$skipped expected_validator_fail=$expected_validator_failed expected_validator_rust_decoded=$expected_validator_rust_decoded expected_validator_rust_errors=$expected_validator_rust_errors expected_rust_fail=$expected_rust_failed passed=$passed image_hook_passed=$image_hook_passed failed=$failed"
  log verify "Report: $report_file"

  if [[ "$failed" -gt 0 ]]; then
    log verify "Failures:"
    printf '  - %s\n' "${failures[@]}"
    return 1
  fi

  if [[ -n "$require_exts" ]]; then
    IFS=',' read -r -a required_list <<< "$require_exts"
    local required_ext normalized
    for required_ext in "${required_list[@]}"; do
      normalized="$(echo "$required_ext" | tr '[:upper:]' '[:lower:]' | sed 's/^\.//')"
      case "$normalized" in
        heif) [[ "$comparable_heif" -gt 0 ]] || fail "Required .heif has no comparable files." ;;
        heic) [[ "$comparable_heic" -gt 0 ]] || fail "Required .heic has no comparable files." ;;
        avif) [[ "$comparable_avif" -gt 0 ]] || fail "Required .avif has no comparable files." ;;
        '') ;;
        *) fail "Unknown required extension: $required_ext" ;;
      esac
    done
  fi

  [[ "$passed" -gt 0 ]] || fail "No comparable files passed."
}

timed_command() {
  local timer_output real rss
  timer_output="$({ /usr/bin/time -lp "$@" >/dev/null; } 2>&1)"
  real="$(awk '/^real /{print $2}' <<<"$timer_output" | tail -n 1)"
  rss="$(awk '/maximum resident set size/{print $1}' <<<"$timer_output" | tail -n 1)"
  [[ -n "$real" && -n "$rss" ]] || return 1
  real="$(float_floor_time "$real")"
  printf '%s %s\n' "$real" "$rss"
}

timed_decode_libheif() {
  clean_output_variants "$2"
  timed_command "$LIBHEIF_DEC_BIN" --quiet "$1" "$2" || return 1
  resolve_output_file "$2" >/dev/null
}

timed_decode_helper() {
  clean_output_variants "$2"
  timed_command "$HELPER_BIN_DIR/heif-decode" --orientation preserve "$1" "$2" || return 1
  resolve_output_file "$2" >/dev/null
}

BENCH_MODE="quick"
BENCH_FILES=10
BENCH_RUNS=3
BENCH_ENFORCE=0
BENCH_CORPUS_DIRS=()

parse_bench_options() {
  BENCH_MODE="$1"
  BENCH_FILES="$2"
  BENCH_RUNS="$3"
  BENCH_ENFORCE=0
  BENCH_CORPUS_DIRS=()
  shift 3
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --quick) BENCH_MODE="quick"; shift ;;
      --full) BENCH_MODE="full"; shift ;;
      --files) BENCH_FILES="$2"; shift 2 ;;
      --runs) BENCH_RUNS="$2"; shift 2 ;;
      --enforce) BENCH_ENFORCE=1; shift ;;
      --corpus-dir) BENCH_CORPUS_DIRS+=("$2"); shift 2 ;;
      -h|--help) return 2 ;;
      *) fail "Unknown benchmark option: $1" ;;
    esac
  done
}

cmd_bench_decode() {
  if ! parse_bench_options quick 10 3 "$@"; then
    cat <<'EOF'
Usage: scripts/heic_tests.sh bench-decode [--quick|--full] [--files n] [--runs n]
       [--enforce] [--corpus-dir dir ...]
EOF
    return 0
  fi
  local mode="$BENCH_MODE" bench_files="$BENCH_FILES" runs="$BENCH_RUNS" enforce="$BENCH_ENFORCE"
  local corpus_dirs=()
  if [[ ${#BENCH_CORPUS_DIRS[@]} -gt 0 ]]; then
    corpus_dirs=("${BENCH_CORPUS_DIRS[@]}")
  fi

  require_cmd bash
  require_cmd find
  require_cmd sort
  require_cmd awk
  require_cmd head
  require_cmd stat
  require_cmd /usr/bin/time
  build_libheif_decoder
  build_helper

  if [[ ${#corpus_dirs[@]} -gt 0 ]]; then
    load_corpus "$mode" 120 "${corpus_dirs[@]}"
  else
    load_corpus "$mode" 120
  fi
  local files=("${LOADED_FILES[@]}")

  local tmp_dir="$TEST_ROOT/bench-decode/tmp"
  rm -rf "$tmp_dir"
  mkdir -p "$tmp_dir"

  local candidates=()
  local input_file ref_out rust_out
  for input_file in "${files[@]}"; do
    ref_out="$tmp_dir/probe.ref.png"
    rust_out="$tmp_dir/probe.rust.png"
    clean_output_variants "$ref_out"
    clean_output_variants "$rust_out"
    "$LIBHEIF_DEC_BIN" --quiet "$input_file" "$ref_out" >/dev/null 2>&1 || continue
    resolve_output_file "$ref_out" >/dev/null || continue
    decode_with_helper "$input_file" "$rust_out" >/dev/null 2>&1 || continue
    resolve_output_file "$rust_out" >/dev/null || continue
    candidates+=("$(file_size "$input_file")::$input_file")
  done
  [[ ${#candidates[@]} -gt 0 ]] || fail "No benchmark candidates could be decoded by both decoders."

  local selected=()
  while IFS= read -r line; do
    selected+=("$line")
  done < <(printf '%s\n' "${candidates[@]}" | sort -rn | head -n "$bench_files")

  local total_lib_time="0" total_rust_time="0" peak_lib_rss=0 peak_rust_rss=0
  log bench "Benchmarking ${#selected[@]} file(s), runs=$runs"

  local entry rel_path lib_sum rust_sum lib_peak rust_peak i lib_time lib_rss rust_time rust_rss
  for entry in "${selected[@]}"; do
    input_file="${entry#*::}"
    rel_path="$(display_path "$input_file")"
    lib_sum="0"
    rust_sum="0"
    lib_peak=0
    rust_peak=0
    for ((i=1; i<=runs; i++)); do
      read -r lib_time lib_rss < <(timed_decode_libheif "$input_file" "$tmp_dir/validator.$i.png")
      read -r rust_time rust_rss < <(timed_decode_helper "$input_file" "$tmp_dir/rust.$i.png")
      lib_sum="$(float_add "$lib_sum" "$lib_time")"
      rust_sum="$(float_add "$rust_sum" "$rust_time")"
      (( lib_rss > lib_peak )) && lib_peak=$lib_rss
      (( rust_rss > rust_peak )) && rust_peak=$rust_rss
    done
    local lib_avg rust_avg ratio
    lib_avg="$(float_div "$lib_sum" "$runs")"
    rust_avg="$(float_div "$rust_sum" "$runs")"
    ratio="$(float_div "$rust_avg" "$lib_avg")"
    total_lib_time="$(float_add "$total_lib_time" "$lib_avg")"
    total_rust_time="$(float_add "$total_rust_time" "$rust_avg")"
    (( lib_peak > peak_lib_rss )) && peak_lib_rss=$lib_peak
    (( rust_peak > peak_rust_rss )) && peak_rust_rss=$rust_peak
    log bench "file=$rel_path validator_avg=${lib_avg}s rust_avg=${rust_avg}s ratio=${ratio}x validator_peak_rss=${lib_peak} rust_peak_rss=${rust_peak}"
  done

  local time_ratio rss_ratio max_slowdown max_rss_multiplier
  max_slowdown="${MAX_SLOWDOWN:-2.5}"
  max_rss_multiplier="${MAX_RSS_MULTIPLIER:-3.0}"
  time_ratio="$(float_div "$total_rust_time" "$total_lib_time")"
  rss_ratio="$(float_div "$peak_rust_rss" "$peak_lib_rss")"
  log bench "Aggregate: rust/validator time ratio=${time_ratio}x peak_rss_ratio=${rss_ratio}x"
  log bench "Thresholds: time<=${max_slowdown}x peak_rss<=${max_rss_multiplier}x"
  if [[ "$enforce" -eq 1 ]]; then
    [[ "$(float_leq "$time_ratio" "$max_slowdown")" -eq 1 && "$(float_leq "$rss_ratio" "$max_rss_multiplier")" -eq 1 ]] ||
      fail "Performance thresholds exceeded (time_ratio=${time_ratio} rss_ratio=${rss_ratio})"
  fi
}

cmd_pair_bench() {
  local label="$1" bin_name="$2" mode_a="$3" mode_b="$4" env_time="$5" env_rss="$6"
  shift 6
  if ! parse_bench_options quick 10 3 "$@"; then
    cat <<EOF
Usage: scripts/heic_tests.sh $label [--quick|--full] [--files n] [--runs n]
       [--enforce] [--corpus-dir dir ...]
EOF
    return 0
  fi
  local mode="$BENCH_MODE" bench_files="$BENCH_FILES" runs="$BENCH_RUNS" enforce="$BENCH_ENFORCE"
  local corpus_dirs=()
  if [[ ${#BENCH_CORPUS_DIRS[@]} -gt 0 ]]; then
    corpus_dirs=("${BENCH_CORPUS_DIRS[@]}")
  fi

  require_cmd find
  require_cmd sort
  require_cmd awk
  require_cmd head
  require_cmd stat
  require_cmd /usr/bin/time
  build_helper

  if [[ ${#corpus_dirs[@]} -gt 0 ]]; then
    load_corpus "$mode" 120 "${corpus_dirs[@]}"
  else
    load_corpus "$mode" 120
  fi
  local files=("${LOADED_FILES[@]}")
  local bin="$HELPER_BIN_DIR/$bin_name"

  local candidates=()
  local input_file
  for input_file in "${files[@]}"; do
    "$bin" "$mode_a" "$input_file" >/dev/null 2>&1 || continue
    "$bin" "$mode_b" "$input_file" >/dev/null 2>&1 || continue
    candidates+=("$(file_size "$input_file")::$input_file")
  done
  [[ ${#candidates[@]} -gt 0 ]] || fail "No benchmark candidates could be decoded by both modes."

  local selected=()
  while IFS= read -r line; do
    selected+=("$line")
  done < <(printf '%s\n' "${candidates[@]}" | sort -rn | head -n "$bench_files")

  local total_a="0" total_b="0" peak_a=0 peak_b=0
  log "$label" "Benchmarking ${#selected[@]} file(s), runs=$runs"
  local entry rel_path sum_a sum_b run time_a rss_a time_b rss_b
  for entry in "${selected[@]}"; do
    input_file="${entry#*::}"
    rel_path="$(display_path "$input_file")"
    sum_a="0"
    sum_b="0"
    local file_peak_a=0 file_peak_b=0
    for ((run=1; run<=runs; run++)); do
      read -r time_a rss_a < <(timed_command "$bin" "$mode_a" "$input_file")
      read -r time_b rss_b < <(timed_command "$bin" "$mode_b" "$input_file")
      sum_a="$(float_add "$sum_a" "$time_a")"
      sum_b="$(float_add "$sum_b" "$time_b")"
      (( rss_a > file_peak_a )) && file_peak_a=$rss_a
      (( rss_b > file_peak_b )) && file_peak_b=$rss_b
    done
    local avg_a avg_b ratio
    avg_a="$(float_div "$sum_a" "$runs")"
    avg_b="$(float_div "$sum_b" "$runs")"
    ratio="$(float_div "$avg_b" "$avg_a")"
    total_a="$(float_add "$total_a" "$avg_a")"
    total_b="$(float_add "$total_b" "$avg_b")"
    (( file_peak_a > peak_a )) && peak_a=$file_peak_a
    (( file_peak_b > peak_b )) && peak_b=$file_peak_b
    log "$label" "file=$rel_path ${mode_a}_avg=${avg_a}s ${mode_b}_avg=${avg_b}s ratio=${ratio}x ${mode_a}_peak_rss=${file_peak_a} ${mode_b}_peak_rss=${file_peak_b}"
  done

  local time_ratio rss_ratio max_time max_rss
  max_time="${!env_time:-2.0}"
  max_rss="${!env_rss:-2.0}"
  time_ratio="$(float_div "$total_b" "$total_a")"
  rss_ratio="$(float_div "$peak_b" "$peak_a")"
  log "$label" "Aggregate: ${mode_b}/${mode_a} time ratio=${time_ratio}x peak_rss_ratio=${rss_ratio}x"
  log "$label" "Thresholds: time<=${max_time}x peak_rss<=${max_rss}x"
  if [[ "$enforce" -eq 1 ]]; then
    [[ "$(float_leq "$time_ratio" "$max_time")" -eq 1 && "$(float_leq "$rss_ratio" "$max_rss")" -eq 1 ]] ||
      fail "$label thresholds exceeded (time_ratio=${time_ratio} rss_ratio=${rss_ratio})"
  fi
}

cmd_bench_stream() {
  local mode="quick" bench_files=6 runs=2 enforce=0 corpus_dirs=()
  local workers="${STREAM_BENCH_WORKERS:-10}" iterations=4
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --quick) mode="quick"; shift ;;
      --full) mode="full"; shift ;;
      --files) bench_files="$2"; shift 2 ;;
      --runs) runs="$2"; shift 2 ;;
      --workers) workers="$2"; shift 2 ;;
      --iterations) iterations="$2"; shift 2 ;;
      --enforce) enforce=1; shift ;;
      --corpus-dir) corpus_dirs+=("$2"); shift 2 ;;
      -h|--help)
        cat <<'EOF'
Usage: scripts/heic_tests.sh bench-stream [--quick|--full] [--files n]
       [--runs n] [--workers n] [--iterations n] [--enforce] [--corpus-dir dir ...]
EOF
        return 0
        ;;
      *) fail "Unknown bench-stream option: $1" ;;
    esac
  done

  require_cmd find
  require_cmd sort
  require_cmd awk
  require_cmd head
  require_cmd stat
  require_cmd /usr/bin/time
  build_helper

  if [[ ${#corpus_dirs[@]} -gt 0 ]]; then
    load_corpus "$mode" 120 "${corpus_dirs[@]}"
  else
    load_corpus "$mode" 120
  fi
  local files=("${LOADED_FILES[@]}")
  local bin="$HELPER_BIN_DIR/heif-stream-concurrency-bench"
  local candidates=()
  local input_file ext
  for input_file in "${files[@]}"; do
    ext="${input_file##*.}"
    [[ "$ext" == "heic" || "$ext" == "HEIC" || "$ext" == "avif" || "$ext" == "AVIF" ]] || continue
    "$bin" path 1 1 "$input_file" >/dev/null 2>&1 || continue
    "$bin" read 1 1 "$input_file" >/dev/null 2>&1 || continue
    candidates+=("$(file_size "$input_file")::$input_file")
  done
  [[ ${#candidates[@]} -gt 0 ]] || fail "No stream benchmark candidates could be decoded by both modes."

  local selected_entries=()
  while IFS= read -r line; do
    selected_entries+=("$line")
  done < <(printf '%s\n' "${candidates[@]}" | sort -rn | head -n "$bench_files")
  local selected_files=()
  local entry
  for entry in "${selected_entries[@]}"; do
    selected_files+=("${entry#*::}")
  done

  log stream "Selected ${#selected_files[@]} file(s):"
  for input_file in "${selected_files[@]}"; do
    log stream "  - $(display_path "$input_file")"
  done

  local overall_peak_rss=0 overall_worst_slowdown="0"
  local stream_mode baseline_sum concurrent_sum baseline_peak concurrent_peak run
  for stream_mode in path read; do
    baseline_sum="0"
    concurrent_sum="0"
    baseline_peak=0
    concurrent_peak=0
    for ((run=1; run<=runs; run++)); do
      read -r baseline_time baseline_rss < <(timed_command "$bin" "$stream_mode" 1 "$iterations" "${selected_files[@]}")
      read -r concurrent_time concurrent_rss < <(timed_command "$bin" "$stream_mode" "$workers" "$iterations" "${selected_files[@]}")
      baseline_sum="$(float_add "$baseline_sum" "$baseline_time")"
      concurrent_sum="$(float_add "$concurrent_sum" "$concurrent_time")"
      (( baseline_rss > baseline_peak )) && baseline_peak=$baseline_rss
      (( concurrent_rss > concurrent_peak )) && concurrent_peak=$concurrent_rss
    done
    local baseline_avg concurrent_avg slowdown baseline_ops concurrent_ops
    baseline_avg="$(float_div "$baseline_sum" "$runs")"
    concurrent_avg="$(float_div "$concurrent_sum" "$runs")"
    slowdown="$(float_div "$concurrent_avg" "$(float_mul "$baseline_avg" "$workers")")"
    baseline_ops="$(float_div "$iterations" "$baseline_avg")"
    concurrent_ops="$(float_div "$((workers * iterations))" "$concurrent_avg")"
    (( concurrent_peak > overall_peak_rss )) && overall_peak_rss=$concurrent_peak
    [[ "$(float_gt "$slowdown" "$overall_worst_slowdown")" -eq 1 ]] && overall_worst_slowdown="$slowdown"
    log stream "mode=$stream_mode baseline_avg=${baseline_avg}s concurrent_avg=${concurrent_avg}s slowdown=${slowdown}x baseline_ops_per_sec=${baseline_ops} concurrent_ops_per_sec=${concurrent_ops} baseline_peak_rss=${baseline_peak} concurrent_peak_rss=${concurrent_peak}"
    echo "METRIC mode=${stream_mode} baseline_avg_s=${baseline_avg} concurrent_avg_s=${concurrent_avg} slowdown_x=${slowdown} baseline_ops_per_sec=${baseline_ops} concurrent_ops_per_sec=${concurrent_ops} baseline_peak_rss=${baseline_peak} concurrent_peak_rss=${concurrent_peak}"
  done

  local peak_mib max_rss_mib max_slowdown
  peak_mib="$(bytes_to_mib "$overall_peak_rss")"
  max_rss_mib="${MAX_STREAM_CONCURRENT_RSS_MIB:-1536}"
  max_slowdown="${MAX_STREAM_CONCURRENT_SLOWDOWN:-2.5}"
  log stream "Aggregate: workers=${workers} iterations=${iterations} runs=${runs} files=${#selected_files[@]} worst_slowdown=${overall_worst_slowdown}x peak_concurrent_rss=${overall_peak_rss}"
  log stream "Thresholds: worst_slowdown<=${max_slowdown}x peak_concurrent_rss<=${max_rss_mib}MiB"
  echo "METRIC aggregate workers=${workers} iterations=${iterations} runs=${runs} files=${#selected_files[@]} worst_slowdown_x=${overall_worst_slowdown} peak_concurrent_rss=${overall_peak_rss} peak_concurrent_rss_mib=${peak_mib}"
  if [[ "$enforce" -eq 1 ]]; then
    [[ "$(float_leq "$overall_worst_slowdown" "$max_slowdown")" -eq 1 && "$(float_leq "$peak_mib" "$max_rss_mib")" -eq 1 ]] ||
      fail "stream thresholds exceeded (worst_slowdown=${overall_worst_slowdown} peak_concurrent_rss_mib=${peak_mib})"
  fi
}

cmd_all() {
  cmd_verify --full --require-exts heic,avif
  cmd_bench_decode --full --files 12 --runs 5
  cmd_pair_bench bench-ingestion heif-ingestion-bench bytes path MAX_INGEST_PATH_SLOWDOWN MAX_INGEST_PATH_RSS_MULTIPLIER --full --files 12 --runs 5
  cmd_pair_bench bench-image heif-image-adapter-bench direct adapter MAX_IMAGE_ADAPTER_SLOWDOWN MAX_IMAGE_ADAPTER_RSS_MULTIPLIER --full --files 12 --runs 5
  cmd_bench_stream --full --files 6 --runs 2 --workers 10 --iterations 4
}

main() {
  local command="${1:-}"
  if [[ -z "$command" || "$command" == "-h" || "$command" == "--help" ]]; then
    usage
    return 0
  fi
  shift
  mkdir -p "$TEST_ROOT"

  case "$command" in
    verify) cmd_verify "$@" ;;
    bench-decode) cmd_bench_decode "$@" ;;
    bench-ingestion) cmd_pair_bench bench-ingestion heif-ingestion-bench bytes path MAX_INGEST_PATH_SLOWDOWN MAX_INGEST_PATH_RSS_MULTIPLIER "$@" ;;
    bench-image) cmd_pair_bench bench-image heif-image-adapter-bench direct adapter MAX_IMAGE_ADAPTER_SLOWDOWN MAX_IMAGE_ADAPTER_RSS_MULTIPLIER "$@" ;;
    bench-stream) cmd_bench_stream "$@" ;;
    all) cmd_all "$@" ;;
    gen-stress) "$ROOT_DIR/scripts/gen_stress_corpus.sh" "$@" ;;
    build-helper) build_helper ;;
    *) fail "Unknown command: $command" ;;
  esac
}

main "$@"
