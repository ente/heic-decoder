#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
REPO_ID="$(printf '%s' "$ROOT_DIR" | shasum -a 256 | awk '{print substr($1, 1, 12)}')"
DEFAULT_STATE_BASE="${XDG_CACHE_HOME:-$HOME/.cache}/heic-decoder-autoresearch"
STATE_DIR="${HEIC_AUTORESEARCH_STATE_DIR:-$DEFAULT_STATE_BASE/$REPO_ID}"
STATE_FILE="$STATE_DIR/state.tsv"
RESULTS_FILE="$STATE_DIR/results.tsv"
STOP_FILE="$STATE_DIR/STOP"
JOURNAL_FILE="${STATE_DIR}.experiments.md"
JOURNAL_MIRROR="$ROOT_DIR/.heic-autoresearch/experiments.md"
BENCHMARK_SOURCE="$ROOT_DIR/autoresearch/benchmark.rs"
BENCHMARK_CORPUS="$ROOT_DIR/autoresearch/benchmark-corpus.txt"
CONFIRMATION_CORPUS="$STATE_DIR/confirmation-corpus.txt"

CHANGED_FILES=()
UNTRACKED_FILES=()
BENCHMARK_FILES=()
CONFIRMATION_FILES=()
PAIR_BASELINE_SCORE=""
PAIR_CANDIDATE_SCORE=""
PAIR_SPEEDUP=""
RUN_LOCK_DIR=""

usage() {
  cat <<'EOF'
Usage: scripts/autoresearch.sh <command> [options]

Commands:
  setup                         Validate and record a fresh clean baseline
  run --hours N [options]       Run unattended one-attempt Codex experiments
  evaluate [--description text] Evaluate current uncommitted source changes
  bench [--samples N]           Benchmark the current worktree without state
  status                        Show champion and recent results
  stop                          Cooperatively stop an unattended run

Run options:
  --hours N                     Wall-clock budget; accepts decimal hours
  --max-experiments N           Optional attempt-count limit
  --model MODEL                 Override the Codex CLI's configured model

The trusted state directory defaults to:
  ~/.cache/heic-decoder-autoresearch/<repo-id>/

The durable experiment journal is mirrored to:
  .heic-autoresearch/experiments.md
EOF
}

log() {
  printf '[autoresearch:%s] %s\n' "$1" "${*:2}"
}

die() {
  log error "$*" >&2
  exit 1
}

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "Missing required command: $1"
}

cleanup_run_lock() {
  if [[ -n "${RUN_LOCK_DIR:-}" ]]; then
    rm -rf "$RUN_LOCK_DIR"
  fi
}

acquire_run_lock() {
  local lock_dir="$1" existing_pid=""
  if ! mkdir "$lock_dir" 2>/dev/null; then
    [[ -f "$lock_dir/pid" ]] && existing_pid="$(cat "$lock_dir/pid" 2>/dev/null || true)"
    if [[ "$existing_pid" =~ ^[0-9]+$ ]] && kill -0 "$existing_pid" 2>/dev/null; then
      die "Another autoresearch run is active with pid $existing_pid."
    fi
    log run "Removing stale run lock: $lock_dir"
    rm -rf "$lock_dir"
    mkdir "$lock_dir" || die "Could not acquire run lock: $lock_dir"
  fi
  printf '%s\n' "$$" > "$lock_dir/pid"
  RUN_LOCK_DIR="$lock_dir"
  trap cleanup_run_lock EXIT
}

sanitize_field() {
  printf '%s' "$1" \
    | tr '\t\r\n' '   ' \
    | sed 's/[[:space:]][[:space:]]*/ /g; s/^ //; s/ $//' \
    | cut -c1-240
}

current_branch() {
  git -C "$ROOT_DIR" symbolic-ref --quiet --short HEAD \
    || die "Autoresearch requires a named git branch."
}

current_commit() {
  git -C "$ROOT_DIR" rev-parse HEAD
}

short_commit() {
  git -C "$ROOT_DIR" rev-parse --short=12 "$1"
}

get_state() {
  local key="$1"
  [[ -f "$STATE_FILE" ]] || die "No autoresearch baseline. Run scripts/autoresearch.sh setup first."
  awk -F '\t' -v key="$key" '$1 == key {sub(/^[^\t]*\t/, ""); print; exit}' "$STATE_FILE"
}

set_state() {
  local key="$1" value="$2" tmp="$STATE_FILE.tmp.$$"
  awk -F '\t' -v OFS='\t' -v key="$key" -v value="$value" '
    $1 == key { print key, value; found = 1; next }
    { print }
    END { if (!found) print key, value }
  ' "$STATE_FILE" > "$tmp"
  mv "$tmp" "$STATE_FILE"
}

ensure_external_state_dir() {
  mkdir -p "$STATE_DIR"
  local physical
  physical="$(cd "$STATE_DIR" && pwd -P)"
  case "$physical/" in
    "$ROOT_DIR/"*)
      die "HEIC_AUTORESEARCH_STATE_DIR must be outside the repository so the optimization agent cannot modify trusted state."
      ;;
  esac
}

mirror_journal() {
  [[ -f "$JOURNAL_FILE" ]] || return 0
  mkdir -p "$(dirname "$JOURNAL_MIRROR")"
  cp "$JOURNAL_FILE" "$JOURNAL_MIRROR"
}

initialize_journal() {
  if [[ ! -f "$JOURNAL_FILE" ]]; then
    mkdir -p "$(dirname "$JOURNAL_FILE")"
    {
      printf '# HEIC decoder autoresearch experiment journal\n\n'
      printf 'This is the controller-owned history of accepted and rejected experiments. '
      printf 'The live repository mirror is `.heic-autoresearch/experiments.md`; '
      printf 'optimization agents may read it but must not edit it.\n'
    } > "$JOURNAL_FILE"
  fi
  mirror_journal
}

require_clean_worktree() {
  if [[ -n "$(git -C "$ROOT_DIR" status --porcelain=v1 --untracked-files=all)" ]]; then
    git -C "$ROOT_DIR" status --short >&2
    die "The worktree must be clean before setup or an unattended run."
  fi
}

require_champion_head() {
  local expected_branch expected_commit
  expected_branch="$(get_state branch)"
  expected_commit="$(get_state champion_commit)"
  [[ "$(current_branch)" == "$expected_branch" ]] \
    || die "Expected branch '$expected_branch'; current branch is '$(current_branch)'."
  [[ "$(current_commit)" == "$expected_commit" ]] \
    || die "HEAD is not the saved champion $(short_commit "$expected_commit"). Run setup for an intentional new baseline."
}

is_libheif_source_dir() {
  [[ -f "$1/CMakeLists.txt" && -d "$1/examples" && -d "$1/tests/data" && -d "$1/fuzzing/data/corpus" ]]
}

absolute_dir() {
  (cd "$1" && pwd -P)
}

absolute_file() {
  local dir base
  dir="$(dirname "$1")"
  base="$(basename "$1")"
  printf '%s/%s\n' "$(absolute_dir "$dir")" "$base"
}

resolve_setup_paths() {
  local source_candidate ente_candidate validator_candidate build_dir
  source_candidate="${HEIC_LIBHEIF_SOURCE_DIR:-$ROOT_DIR/.heic-test-assets/libheif}"
  if ! is_libheif_source_dir "$source_candidate" && is_libheif_source_dir "$ROOT_DIR/.heic-test-assets"; then
    source_candidate="$ROOT_DIR/.heic-test-assets"
  fi
  is_libheif_source_dir "$source_candidate" \
    || die "No libheif source/corpus checkout found. Follow TESTING.md first."
  SETUP_LIBHEIF_SOURCE="$(absolute_dir "$source_candidate")"

  ente_candidate="${HEIC_ENTE_FIXTURES_DIR:-$ROOT_DIR/.heic-test-assets/ente-test-fixtures}"
  [[ -d "$ente_candidate/media/heic/v1/files" ]] \
    || die "The Ente HEIC fixture corpus is missing. Follow TESTING.md first."
  SETUP_ENTE_DIR="$(absolute_dir "$ente_candidate")"

  [[ -d "$ROOT_DIR/.heic-test-assets/stress-corpus" ]] \
    || die "The stress corpus is missing. Run scripts/heic_tests.sh gen-stress first."
  SETUP_STRESS_DIR="$(absolute_dir "$ROOT_DIR/.heic-test-assets/stress-corpus")"

  build_dir="${LIBHEIF_BUILD_DIR:-$ROOT_DIR/.heic-test-runs/validator-build}"
  validator_candidate="${LIBHEIF_DEC_BIN:-$build_dir/examples/heif-dec}"
  SETUP_VALIDATOR="$validator_candidate"
}

load_benchmark_files() {
  BENCHMARK_FILES=()
  local line path
  while IFS= read -r line || [[ -n "$line" ]]; do
    line="${line%%#*}"
    line="$(sanitize_field "$line")"
    [[ -n "$line" ]] || continue
    case "$line" in
      /*) path="$line" ;;
      *) path="$ROOT_DIR/$line" ;;
    esac
    [[ -f "$path" ]] || die "Benchmark input is missing: $path"
    BENCHMARK_FILES+=("$path")
  done < "$BENCHMARK_CORPUS"
  [[ ${#BENCHMARK_FILES[@]} -gt 0 ]] || die "The benchmark corpus manifest is empty."
}

load_confirmation_files() {
  CONFIRMATION_FILES=()
  local path
  [[ -s "$CONFIRMATION_CORPUS" ]] \
    || die "Pinned full-corpus hook benchmark is missing. Run setup again."
  while IFS= read -r path || [[ -n "$path" ]]; do
    [[ -n "$path" ]] || continue
    [[ -f "$path" ]] || die "Confirmation benchmark input is missing: $path"
    CONFIRMATION_FILES+=("$path")
  done < "$CONFIRMATION_CORPUS"
  [[ ${#CONFIRMATION_FILES[@]} -gt 0 ]] \
    || die "The confirmation benchmark corpus is empty."
}

write_corpus_dirs() {
  printf '%s\n' \
    "$SETUP_LIBHEIF_SOURCE/examples" \
    "$SETUP_LIBHEIF_SOURCE/tests/data" \
    "$SETUP_LIBHEIF_SOURCE/fuzzing/data/corpus" \
    "$SETUP_ENTE_DIR/media/heic/v1/files" \
    "$SETUP_STRESS_DIR" > "$STATE_DIR/corpus-dirs.txt"
}

hash_corpus() {
  local output="$1" paths_file="$STATE_DIR/corpus-paths.tmp.$$" dir path hash
  : > "$paths_file"
  while IFS= read -r dir; do
    find "$dir" -type f \( -iname '*.heif' -o -iname '*.heic' -o -iname '*.avif' \) -print
  done < "$STATE_DIR/corpus-dirs.txt" | LC_ALL=C sort -u > "$paths_file"
  [[ -s "$paths_file" ]] || die "The correctness corpus is empty."
  : > "$output"
  while IFS= read -r path; do
    hash="$(shasum -a 256 "$path" | awk '{print $1}')"
    printf '%s  %s\n' "$hash" "$path" >> "$output"
  done < "$paths_file"
  rm -f "$paths_file"
}

capture_asset_integrity() {
  hash_corpus "$STATE_DIR/corpus.sha256"
  local validator validator_hash
  validator="$SETUP_VALIDATOR"
  [[ -x "$validator" ]] || die "Validator binary was not produced at $validator"
  validator="$(absolute_file "$validator")"
  validator_hash="$(shasum -a 256 "$validator" | awk '{print $1}')"
  set_state libheif_source "$SETUP_LIBHEIF_SOURCE"
  set_state ente_fixtures_dir "$SETUP_ENTE_DIR"
  set_state validator_path "$validator"
  set_state validator_sha256 "$validator_hash"
}

verify_asset_integrity() {
  local current="$STATE_DIR/corpus.current.$$" validator expected actual
  hash_corpus "$current"
  if ! cmp -s "$STATE_DIR/corpus.sha256" "$current"; then
    diff -u "$STATE_DIR/corpus.sha256" "$current" >&2 || true
    rm -f "$current"
    die "Correctness corpus changed since setup. Restore it or establish a deliberate fresh baseline."
  fi
  rm -f "$current"

  validator="$(get_state validator_path)"
  expected="$(get_state validator_sha256)"
  [[ -x "$validator" ]] || die "Pinned validator binary is missing: $validator"
  actual="$(shasum -a 256 "$validator" | awk '{print $1}')"
  [[ "$actual" == "$expected" ]] \
    || die "Pinned validator binary changed since setup. Restore it or establish a fresh baseline."
}

verify_champion_binary() {
  local champion="$STATE_DIR/champion-bench" expected actual
  expected="$(get_state champion_sha256)"
  [[ -x "$champion" ]] || die "Trusted champion benchmark binary is missing: $champion"
  actual="$(shasum -a 256 "$champion" | awk '{print $1}')"
  [[ -n "$expected" && "$actual" == "$expected" ]] \
    || die "Trusted champion benchmark binary changed; establish a fresh baseline."
}

check_environment_matches_setup() {
  [[ "$(rustc --version)" == "$(get_state rustc_version)" ]] \
    || die "rustc changed since setup; establish a fresh baseline."
  [[ "${RUSTFLAGS:-}" == "$(get_state rustflags)" ]] \
    || die "RUSTFLAGS changed since setup; establish a fresh baseline."
  [[ "$(uname -m)" == "$(get_state architecture)" ]] \
    || die "Machine architecture changed since setup; establish a fresh baseline."
}

prepare_benchmark_project() {
  local project_dir="$STATE_DIR/build/benchmark-project"
  rm -rf "$project_dir"
  mkdir -p "$project_dir/src" "$STATE_DIR/build/target"
  cp "$BENCHMARK_SOURCE" "$project_dir/src/main.rs"
  cp "$ROOT_DIR/Cargo.lock" "$project_dir/Cargo.lock"
  printf '%s\n' \
    '[workspace]' \
    '' \
    '[package]' \
    'name = "heic-autoresearch-bench"' \
    'version = "0.0.0"' \
    'edition = "2024"' \
    'publish = false' \
    '' \
    '[dependencies]' \
    "heic_decoder = { path = \"$ROOT_DIR\", features = [\"image-integration\"] }" \
    'image = { version = "0.25.10", default-features = false }' \
    > "$project_dir/Cargo.toml"
}

build_benchmark_binary() {
  local destination="$1" build_log="$2"
  prepare_benchmark_project
  if ! CARGO_TARGET_DIR="$STATE_DIR/build/target" \
    cargo build --manifest-path "$STATE_DIR/build/benchmark-project/Cargo.toml" --release \
      >"$build_log" 2>&1; then
    tail -n 80 "$build_log" >&2 || true
    return 1
  fi
  cp "$STATE_DIR/build/target/release/heic-autoresearch-bench" "$destination"
  chmod 755 "$destination"
}

benchmark_score() {
  local binary="$1" output="$2" warmup="$3" samples="$4" corpus="$5" score
  local benchmark_status=0
  case "$corpus" in
    primary)
      "$binary" --warmup "$warmup" --samples "$samples" "${BENCHMARK_FILES[@]}" \
        >"$output" 2>&1 || benchmark_status=$?
      ;;
    confirmation)
      "$binary" --warmup "$warmup" --samples "$samples" "${CONFIRMATION_FILES[@]}" \
        >"$output" 2>&1 || benchmark_status=$?
      ;;
    *) die "Unknown benchmark corpus: $corpus" ;;
  esac
  if [[ "$benchmark_status" -ne 0 ]]; then
    tail -n 80 "$output" >&2 || true
    return 1
  fi
  score="$(awk '/^score_ms: / {print $2; exit}' "$output")"
  [[ "$score" =~ ^[0-9]+([.][0-9]+)?$ ]] || return 1
  printf '%s\n' "$score"
}

benchmark_pair() {
  local champion="$1" candidate="$2" log_dir="$3" corpus="$4" samples="$5"
  local b1 c1 c2 b2 fingerprints
  [[ "$samples" =~ ^[1-9][0-9]*$ ]] \
    || die "HEIC_AUTORESEARCH_PAIR_SAMPLES must be a positive integer."
  mkdir -p "$log_dir"
  log bench "Interleaved A/B $corpus benchmark (samples per invocation=$samples)"
  # Bring the machine and both executables out of their cold-start state before
  # the A/B/B/A sequence. These scores are deliberately discarded.
  benchmark_score "$champion" "$log_dir/preheat-baseline.log" 0 1 "$corpus" >/dev/null || return 1
  benchmark_score "$candidate" "$log_dir/preheat-candidate.log" 0 1 "$corpus" >/dev/null || return 1
  b1="$(benchmark_score "$champion" "$log_dir/baseline-1.log" 0 "$samples" "$corpus")" || return 1
  c1="$(benchmark_score "$candidate" "$log_dir/candidate-1.log" 0 "$samples" "$corpus")" || return 1
  c2="$(benchmark_score "$candidate" "$log_dir/candidate-2.log" 0 "$samples" "$corpus")" || return 1
  b2="$(benchmark_score "$champion" "$log_dir/baseline-2.log" 0 "$samples" "$corpus")" || return 1
  fingerprints="$(awk '/^suite_fingerprint: / {print $2}' "$log_dir"/*.log | LC_ALL=C sort -u)"
  [[ "$(printf '%s\n' "$fingerprints" | awk 'NF {count++} END {print count + 0}')" -eq 1 ]] \
    || { log bench "Benchmark output fingerprints differed across A/B runs." >&2; return 1; }
  PAIR_BASELINE_SCORE="$(awk -v a="$b1" -v b="$b2" 'BEGIN {printf "%.6f", (a + b) / 2}')"
  PAIR_CANDIDATE_SCORE="$(awk -v a="$c1" -v b="$c2" 'BEGIN {printf "%.6f", (a + b) / 2}')"
  PAIR_SPEEDUP="$(awk -v base="$PAIR_BASELINE_SCORE" -v candidate="$PAIR_CANDIDATE_SCORE" \
    'BEGIN {printf "%.6f", base / candidate}')"
  log bench "champion=${PAIR_BASELINE_SCORE}ms candidate=${PAIR_CANDIDATE_SCORE}ms speedup=${PAIR_SPEEDUP}x"
}

prepare_confirmation_corpus() {
  local champion="$1" log_file="$2" candidates_file="$STATE_DIR/confirmation-candidates.txt"
  local candidates=() path
  sed 's/^[0-9a-fA-F]*  //' "$STATE_DIR/corpus.sha256" \
    | awk 'tolower($0) ~ /\.(heic|heif)$/ {print}' \
    > "$candidates_file"
  while IFS= read -r path; do
    [[ -n "$path" ]] && candidates+=("$path")
  done < "$candidates_file"
  [[ ${#candidates[@]} -gt 0 ]] || die "No HEIC/HEIF files exist in the pinned corpus."
  if ! "$champion" --probe-compatible "${candidates[@]}" > "$log_file" 2>&1; then
    tail -n 80 "$log_file" >&2 || true
    return 1
  fi
  sed -n 's/^compatible: //p' "$log_file" > "$CONFIRMATION_CORPUS"
  [[ -s "$CONFIRMATION_CORPUS" ]] || return 1
  load_confirmation_files
  set_state confirmation_file_count "${#CONFIRMATION_FILES[@]}"
  log setup "Pinned ${#CONFIRMATION_FILES[@]} hook-decodable HEIC/HEIF files for confirmation"
}

collect_changed_files() {
  CHANGED_FILES=()
  UNTRACKED_FILES=()
  local path
  while IFS= read -r -d '' path; do
    CHANGED_FILES+=("$path")
  done < <(git -C "$ROOT_DIR" diff --name-only -z HEAD --)
  while IFS= read -r -d '' path; do
    CHANGED_FILES+=("$path")
    UNTRACKED_FILES+=("$path")
  done < <(git -C "$ROOT_DIR" ls-files --others --exclude-standard -z)
}

changes_are_allowed() {
  local path
  [[ ${#CHANGED_FILES[@]} -gt 0 ]] || return 1
  for path in "${CHANGED_FILES[@]}"; do
    case "$path" in
      src/*.rs|Cargo.toml|Cargo.lock) ;;
      *) log gate "Disallowed changed file: $path" >&2; return 2 ;;
    esac
  done
}

archive_candidate_patch() {
  local attempt="$1" destination="$STATE_DIR/rejected/$attempt.diff" path
  mkdir -p "$STATE_DIR/rejected"
  git -C "$ROOT_DIR" diff --binary HEAD -- > "$destination"
  if [[ ${#UNTRACKED_FILES[@]} -gt 0 ]]; then
    for path in "${UNTRACKED_FILES[@]}"; do
      git -C "$ROOT_DIR" diff --no-index --binary -- /dev/null "$path" >> "$destination" 2>/dev/null || true
    done
  fi
  printf '%s\n' "$destination"
}

discard_candidate_changes() {
  local path
  for path in "${CHANGED_FILES[@]}"; do
    if git -C "$ROOT_DIR" ls-files --error-unmatch -- "$path" >/dev/null 2>&1; then
      git -C "$ROOT_DIR" restore --source=HEAD --staged --worktree -- "$path"
    else
      rm -f "$ROOT_DIR/$path"
    fi
  done
  if [[ -n "$(git -C "$ROOT_DIR" status --porcelain=v1 --untracked-files=all)" ]]; then
    git -C "$ROOT_DIR" status --short >&2
    die "Could not return to the clean champion after discarding a candidate."
  fi
}

append_result() {
  local attempt="$1" commit="$2" primary_score="$3" primary_speedup="$4"
  local confirmation_score="$5" confirmation_speedup="$6" cumulative="$7"
  local status="$8" description="$9"
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" "$attempt" "$commit" \
    "$primary_score" "$primary_speedup" "$confirmation_score" "$confirmation_speedup" \
    "$cumulative" "$status" "$(sanitize_field "$description")" >> "$RESULTS_FILE"
}

journal_learning() {
  local status="$1" reason="$2" primary_speedup="$3" confirmation_speedup="$4"
  case "$status" in
    accepted)
      printf 'The candidate cleared both production image-hook speed gates and all promotion checks. '
      printf 'It is preserved in the recorded commit and becomes the next champion.'
      ;;
    baseline)
      printf 'This entry establishes a measurement baseline; it is not an optimization attempt.'
      ;;
    rejected)
      case "$reason" in
        *"primary hook improvement"*)
          printf 'The implementation measured %s on the primary production image-hook benchmark, ' "x${primary_speedup}"
          printf 'which did not clear the controller\x27s required latency reduction. '
          printf 'Do not retry this implementation unchanged; revisit the idea only with a materially different mechanism or evidence that it affects more of the measured path.'
          ;;
        *"full-corpus hook confirmation"*)
          printf 'The primary benchmark cleared its gate, but the full-corpus confirmation measured %s and did not clear its gate. ' "x${confirmation_speedup}"
          printf 'The gain was not broad or stable enough across the pinned corpus; do not repeat the same implementation unchanged.'
          ;;
        *"correctness failed"*)
          printf 'The candidate was fast enough to promote but failed the full pixel-exact oracle. '
          printf 'Any follow-up must first explain and fix the recorded correctness mismatch; speed alone cannot rescue this implementation.'
          ;;
        *)
          printf 'The controller rejected this implementation because %s. ' "$reason"
          printf 'Consult the attempt artifacts before revisiting it, and do not retry it unchanged without directly addressing that failure.'
          ;;
      esac
      ;;
    no_change)
      printf 'The agent produced no evaluable source change, so no performance factor exists. '
      printf 'A follow-up needs a concrete implementation rather than repeating the same exploration.'
      ;;
    crash)
      printf 'The agent or evaluator exited before a trustworthy performance result was available. '
      printf 'Inspect the attempt log and address that failure before retrying the underlying idea.'
      ;;
  esac
}

append_journal_entry() {
  local attempt="$1" status="$2" description="$3" reason="$4" commit="$5"
  local primary_score="$6" primary_speedup="$7"
  local confirmation_score="$8" confirmation_speedup="$9" cumulative="${10}"
  local patch="${11:-}" timestamp agent_report
  timestamp="$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
  agent_report="$STATE_DIR/attempts/$attempt/agent-last.txt"
  initialize_journal

  {
    printf '\n## %s — attempt %s — %s\n\n' "$timestamp" "$attempt" "$status"
    printf -- '- Experiment: %s\n' "$(sanitize_field "$description")"
    printf -- '- Primary production-hook speed factor: '
    if [[ "$primary_speedup" == "-" ]]; then
      printf 'not measured\n'
    else
      printf '`x%s` (candidate `%s ms`)\n' "$primary_speedup" "$primary_score"
    fi
    printf -- '- Full-corpus production-hook speed factor: '
    if [[ "$confirmation_speedup" == "-" ]]; then
      printf 'not measured\n'
    else
      printf '`x%s` (candidate `%s ms`)\n' "$confirmation_speedup" "$confirmation_score"
    fi
    printf -- '- Cumulative speed factor for this baseline: `x%s`\n' "$cumulative"
    [[ "$commit" == "-" ]] || printf -- '- Commit: `%s`\n' "$commit"
    [[ -z "$reason" ]] || printf -- '- Controller result: %s\n' "$reason"
    if [[ -n "$patch" ]]; then
      printf -- '- Rejected patch: `rejected/%s.diff` in this run\x27s trusted-state directory or archive\n' "$attempt"
    fi
    printf '\n### Learning\n\n'
    journal_learning "$status" "$reason" "$primary_speedup" "$confirmation_speedup"
    printf '\n'
    if [[ -s "$agent_report" ]]; then
      printf '\n### Agent report\n\n'
      while IFS= read -r line || [[ -n "$line" ]]; do
        printf '> %s\n' "$line"
      done < "$agent_report"
    fi
  } >> "$JOURNAL_FILE"
  mirror_journal
}

record_result() {
  local attempt="$1" commit="$2" primary_score="$3" primary_speedup="$4"
  local confirmation_score="$5" confirmation_speedup="$6" cumulative="$7"
  local ledger_status="$8" description="$9" reason="${10:-}" patch="${11:-}"
  local journal_status ledger_description
  ledger_description="$description"
  [[ -z "$reason" ]] || ledger_description="$reason; $description"
  case "$ledger_status" in
    keep)
      if [[ "$attempt" == "baseline" ]]; then journal_status=baseline; else journal_status=accepted; fi
      ;;
    discard) journal_status=rejected ;;
    no_change) journal_status=no_change ;;
    crash) journal_status=crash ;;
    *) journal_status="$ledger_status" ;;
  esac
  append_result "$attempt" "$commit" "$primary_score" "$primary_speedup" \
    "$confirmation_score" "$confirmation_speedup" "$cumulative" \
    "$ledger_status" "$ledger_description"
  append_journal_entry "$attempt" "$journal_status" "$description" "$reason" "$commit" \
    "$primary_score" "$primary_speedup" "$confirmation_score" \
    "$confirmation_speedup" "$cumulative" "$patch"
}

reject_candidate() {
  local attempt="$1" description="$2" reason="$3"
  local primary_score="${4:--}" primary_speedup="${5:--}"
  local confirmation_score="${6:--}" confirmation_speedup="${7:--}"
  local patch cumulative
  collect_changed_files
  patch="$(archive_candidate_patch "$attempt")"
  cumulative="$(get_state cumulative_speedup)"
  record_result "$attempt" - "$primary_score" "$primary_speedup" \
    "$confirmation_score" "$confirmation_speedup" "$cumulative" discard \
    "$description" "$reason" "$patch"
  discard_candidate_changes
  log discard "$reason (patch: $patch)"
  return 2
}

check_no_native_links() {
  local metadata="$1" error_log="${1%.json}.metadata.stderr.log" linked
  if ! cargo metadata --format-version 1 --locked > "$metadata" 2> "$error_log"; then
    tail -n 80 "$error_log" >&2 || true
    return 1
  fi
  # rayon-core deliberately uses `links = "rayon-core"` without linking any
  # native code; its no-op build script only makes Cargo enforce one runtime
  # instance. Keep that exact crates.io package as the sole audited exception.
  # Any other package declaring `links` remains forbidden.
  if ! linked="$(jq -r '
      .packages[]
      | select(.links != null)
      | select(
          .name != "rayon-core"
          or .links != "rayon-core"
          or .source != "registry+https://github.com/rust-lang/crates.io-index"
        )
      | "\(.name) \(.version) links=\(.links)"
    ' "$metadata")"; then
    log gate "Could not inspect Cargo metadata for native linkage." >&2
    return 1
  fi
  if [[ -n "$linked" ]]; then
    printf '%s\n' "$linked" >&2
    return 1
  fi
}

run_portability_checks() {
  local log_file="$1" configured installed target
  configured="${HEIC_AUTORESEARCH_CHECK_TARGETS:-aarch64-apple-ios,aarch64-linux-android,wasm32-unknown-unknown}"
  installed="$(rustup target list --installed 2>/dev/null || true)"
  : > "$log_file"
  IFS=',' read -r -a targets <<< "$configured"
  if [[ ${#targets[@]} -gt 0 ]]; then
    for target in "${targets[@]}"; do
      [[ -n "$target" ]] || continue
      if ! grep -qx "$target" <<< "$installed"; then
        log portability "Skipping uninstalled target $target"
        continue
      fi
      log portability "Checking $target"
      if ! cargo check --lib --all-features --locked --target "$target" >> "$log_file" 2>&1; then
        tail -n 80 "$log_file" >&2 || true
        return 1
      fi
    done
  fi
}

run_full_correctness() {
  local log_file="$1" source ente validator
  source="$(get_state libheif_source)"
  ente="$(get_state ente_fixtures_dir)"
  validator="$(get_state validator_path)"
  # The helper is generated test output. Rebuilding it prevents any stale or
  # externally altered ignored binary from participating in the trusted gate.
  rm -rf "$ROOT_DIR/.heic-test-runs/helper"
  if ! HEIC_LIBHEIF_SOURCE_DIR="$source" \
      HEIC_ENTE_FIXTURES_DIR="$ente" \
      LIBHEIF_DEC_BIN="$validator" \
      "$ROOT_DIR/scripts/heic_tests.sh" verify --full --require-exts heic,avif \
      > "$log_file" 2>&1; then
    tail -n 100 "$log_file" >&2 || true
    return 1
  fi
  tail -n 4 "$log_file"
}

evaluate_candidate() {
  local attempt="$1" description="$2"
  local attempt_dir="$STATE_DIR/attempts/$attempt"
  local candidate_binary="$attempt_dir/candidate-bench"
  local champion_binary="$STATE_DIR/champion-bench"
  local min_improvement="${HEIC_AUTORESEARCH_MIN_IMPROVEMENT:-0.05}"
  local confirmation_min_improvement="${HEIC_AUTORESEARCH_CONFIRM_MIN_IMPROVEMENT:-0.05}"
  local primary_samples="${HEIC_AUTORESEARCH_PAIR_SAMPLES:-2}"
  local confirmation_samples="${HEIC_AUTORESEARCH_CONFIRM_SAMPLES:-3}"
  local allowed_ratio confirmation_allowed_ratio cumulative commit message
  local primary_baseline_score primary_candidate_score primary_speedup
  local confirmation_candidate_score confirmation_speedup
  mkdir -p "$attempt_dir"
  initialize_journal

  awk -v improvement="$min_improvement" \
    'BEGIN {exit(improvement >= 0 && improvement < 1 ? 0 : 1)}' \
    || die "HEIC_AUTORESEARCH_MIN_IMPROVEMENT must be a fraction in [0, 1)."
  awk -v improvement="$confirmation_min_improvement" \
    'BEGIN {exit(improvement >= 0 && improvement < 1 ? 0 : 1)}' \
    || die "HEIC_AUTORESEARCH_CONFIRM_MIN_IMPROVEMENT must be a fraction in [0, 1)."

  require_champion_head
  check_environment_matches_setup
  verify_asset_integrity
  verify_champion_binary
  load_confirmation_files
  collect_changed_files
  local change_status=0
  changes_are_allowed || change_status=$?
  if [[ "$change_status" -ne 0 ]]; then
    if [[ "$change_status" -eq 1 ]]; then
      record_result "$attempt" - - - - - "$(get_state cumulative_speedup)" \
        no_change "$description" "agent made no source changes"
      log discard "Agent made no source changes."
      return 2
    fi
    reject_candidate "$attempt" "$description" "changed files outside the experiment surface"
    return 2
  fi

  if ! git -C "$ROOT_DIR" diff --check HEAD -- > "$attempt_dir/diff-check.log" 2>&1; then
    reject_candidate "$attempt" "$description" "git diff check failed"
    return 2
  fi
  if ! cargo fmt --all -- --check > "$attempt_dir/fmt.log" 2>&1; then
    reject_candidate "$attempt" "$description" "cargo fmt check failed"
    return 2
  fi
  if ! cargo test --all-features --locked > "$attempt_dir/tests.log" 2>&1; then
    tail -n 80 "$attempt_dir/tests.log" >&2 || true
    reject_candidate "$attempt" "$description" "Rust tests failed"
    return 2
  fi
  if ! check_no_native_links "$attempt_dir/metadata.json"; then
    reject_candidate "$attempt" "$description" "dependency graph contains native linkage or metadata failed"
    return 2
  fi
  if ! build_benchmark_binary "$candidate_binary" "$attempt_dir/build-benchmark.log"; then
    reject_candidate "$attempt" "$description" "candidate benchmark build failed"
    return 2
  fi
  if ! benchmark_pair "$champion_binary" "$candidate_binary" \
      "$attempt_dir/benchmark-primary" primary "$primary_samples"; then
    reject_candidate "$attempt" "$description" "candidate primary hook benchmark crashed"
    return 2
  fi
  primary_baseline_score="$PAIR_BASELINE_SCORE"
  primary_candidate_score="$PAIR_CANDIDATE_SCORE"
  primary_speedup="$PAIR_SPEEDUP"

  allowed_ratio="$(awk -v improvement="$min_improvement" 'BEGIN {printf "%.9f", 1 - improvement}')"
  if ! awk -v candidate="$primary_candidate_score" -v baseline="$primary_baseline_score" -v ratio="$allowed_ratio" \
      'BEGIN {exit(candidate <= baseline * ratio ? 0 : 1)}'; then
    reject_candidate "$attempt" "$description" \
      "did not clear the ${min_improvement} primary hook improvement" \
      "$primary_candidate_score" "$primary_speedup"
    return 2
  fi

  log gate "Candidate is faster; running promotion checks."
  if ! cargo clippy --all-targets --all-features --locked -- -D warnings \
      > "$attempt_dir/clippy.log" 2>&1; then
    tail -n 80 "$attempt_dir/clippy.log" >&2 || true
    reject_candidate "$attempt" "$description" "clippy failed" "$primary_candidate_score" "$primary_speedup"
    return 2
  fi
  if ! run_portability_checks "$attempt_dir/portability.log"; then
    reject_candidate "$attempt" "$description" "portability check failed" "$primary_candidate_score" "$primary_speedup"
    return 2
  fi
  verify_asset_integrity
  if ! run_full_correctness "$attempt_dir/correctness.log"; then
    reject_candidate "$attempt" "$description" "full pixel-exact correctness failed" "$primary_candidate_score" "$primary_speedup"
    return 2
  fi

  log gate "Correctness passed; running the pinned full-corpus hook confirmation benchmark."
  if ! benchmark_pair "$champion_binary" "$candidate_binary" \
      "$attempt_dir/benchmark-confirmation" confirmation "$confirmation_samples"; then
    reject_candidate "$attempt" "$description" "candidate confirmation hook benchmark crashed" \
      "$primary_candidate_score" "$primary_speedup"
    return 2
  fi
  confirmation_candidate_score="$PAIR_CANDIDATE_SCORE"
  confirmation_speedup="$PAIR_SPEEDUP"
  confirmation_allowed_ratio="$(awk -v improvement="$confirmation_min_improvement" \
    'BEGIN {printf "%.9f", 1 - improvement}')"
  if ! awk -v candidate="$PAIR_CANDIDATE_SCORE" -v baseline="$PAIR_BASELINE_SCORE" \
      -v ratio="$confirmation_allowed_ratio" \
      'BEGIN {exit(candidate <= baseline * ratio ? 0 : 1)}'; then
    reject_candidate "$attempt" "$description" \
      "did not clear the ${confirmation_min_improvement} full-corpus hook confirmation" \
      "$primary_candidate_score" "$primary_speedup" \
      "$confirmation_candidate_score" "$confirmation_speedup"
    return 2
  fi

  cumulative="$(awk -v old="$(get_state cumulative_speedup)" -v factor="$confirmation_speedup" \
    'BEGIN {printf "%.6f", old * factor}')"
  collect_changed_files
  changes_are_allowed || die "Candidate changed its file scope during evaluation."
  git -C "$ROOT_DIR" add -- "${CHANGED_FILES[@]}"
  message="$(sanitize_field "$description")"
  [[ -n "$message" ]] || message="accepted decoder optimization"
  message="${message:0:68}"
  if ! git -C "$ROOT_DIR" commit -m "perf(autoresearch): $message" \
      > "$attempt_dir/commit.log" 2>&1; then
    git -C "$ROOT_DIR" restore --staged -- "${CHANGED_FILES[@]}" || true
    reject_candidate "$attempt" "$description" "git commit failed" \
      "$primary_candidate_score" "$primary_speedup" \
      "$confirmation_candidate_score" "$confirmation_speedup"
    return 2
  fi
  commit="$(current_commit)"
  cp "$candidate_binary" "$champion_binary"
  set_state champion_sha256 "$(shasum -a 256 "$champion_binary" | awk '{print $1}')"
  set_state champion_commit "$commit"
  set_state champion_score_ms "$primary_candidate_score"
  set_state champion_confirmation_score_ms "$confirmation_candidate_score"
  set_state cumulative_speedup "$cumulative"
  record_result "$attempt" "$(short_commit "$commit")" \
    "$primary_candidate_score" "$primary_speedup" \
    "$confirmation_candidate_score" "$confirmation_speedup" \
    "$cumulative" keep "$description"
  require_clean_worktree
  log keep "Committed $(short_commit "$commit"); estimated cumulative speedup=${cumulative}x"
}

cmd_setup() {
  require_cmd awk
  require_cmd cargo
  require_cmd cmp
  require_cmd codex
  require_cmd find
  require_cmd git
  require_cmd jq
  require_cmd rustc
  require_cmd rustup
  require_cmd shasum
  require_cmd sort
  ensure_external_state_dir
  initialize_journal
  require_clean_worktree
  load_benchmark_files
  resolve_setup_paths

  if [[ -e "$STATE_FILE" ]]; then
    local archive="${STATE_DIR}.archive.$(date -u '+%Y%m%dT%H%M%SZ')"
    mv "$STATE_DIR" "$archive"
    log setup "Archived previous trusted state to $archive"
    mkdir -p "$STATE_DIR"
  fi

  printf '%s\t%s\n' \
    repo_root "$ROOT_DIR" \
    branch "$(current_branch)" \
    champion_commit "$(current_commit)" \
    rustc_version "$(rustc --version)" \
    rustflags "${RUSTFLAGS:-}" \
    architecture "$(uname -m)" \
    created_at "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" \
    next_attempt 1 \
    cumulative_speedup 1.000000 \
    > "$STATE_FILE"
  printf 'timestamp\tattempt\tcommit\tprimary_score_ms\tprimary_speedup\tconfirmation_score_ms\tconfirmation_speedup\tcumulative_speedup\tstatus\tdescription\n' \
    > "$RESULTS_FILE"
  write_corpus_dirs

  log setup "Running baseline Rust tests"
  cargo test --all-features --locked > "$STATE_DIR/setup-tests.log" 2>&1 \
    || { tail -n 100 "$STATE_DIR/setup-tests.log" >&2; die "Baseline Rust tests failed."; }
  log setup "Running the complete baseline correctness oracle"
  if ! HEIC_LIBHEIF_SOURCE_DIR="$SETUP_LIBHEIF_SOURCE" \
      HEIC_ENTE_FIXTURES_DIR="$SETUP_ENTE_DIR" \
      LIBHEIF_BUILD_DIR="${LIBHEIF_BUILD_DIR:-$ROOT_DIR/.heic-test-runs/validator-build}" \
      LIBHEIF_DEC_BIN="$SETUP_VALIDATOR" \
      "$ROOT_DIR/scripts/heic_tests.sh" verify --full --require-exts heic,avif \
      > "$STATE_DIR/setup-correctness.log" 2>&1; then
    tail -n 100 "$STATE_DIR/setup-correctness.log" >&2 || true
    die "Baseline correctness failed."
  fi
  capture_asset_integrity

  log setup "Building and measuring the baseline benchmark"
  build_benchmark_binary "$STATE_DIR/champion-bench" "$STATE_DIR/setup-benchmark-build.log" \
    || die "Could not build the baseline benchmark."
  set_state champion_sha256 "$(shasum -a 256 "$STATE_DIR/champion-bench" | awk '{print $1}')"
  local score confirmation_score
  score="$(benchmark_score "$STATE_DIR/champion-bench" "$STATE_DIR/setup-benchmark.log" 1 3 primary)" \
    || die "Baseline benchmark failed."
  prepare_confirmation_corpus "$STATE_DIR/champion-bench" "$STATE_DIR/setup-confirmation-probe.log" \
    || die "Could not build the pinned full-corpus hook confirmation set."
  confirmation_score="$(benchmark_score "$STATE_DIR/champion-bench" \
    "$STATE_DIR/setup-confirmation-benchmark.log" 1 3 confirmation)" \
    || die "Baseline confirmation benchmark failed."
  set_state initial_score_ms "$score"
  set_state champion_score_ms "$score"
  set_state initial_confirmation_score_ms "$confirmation_score"
  set_state champion_confirmation_score_ms "$confirmation_score"
  record_result baseline "$(short_commit "$(current_commit)")" \
    "$score" 1.000000 "$confirmation_score" 1.000000 1.000000 keep baseline
  log setup "Ready on $(current_branch) at $(short_commit "$(current_commit)"); primary=${score}ms confirmation=${confirmation_score}ms"
  log setup "Trusted state: $STATE_DIR"
}

first_nonempty_line() {
  awk 'NF {print; exit}' "$1" 2>/dev/null || true
}

run_agent_attempt() {
  local attempt="$1" model="$2" prompt_file="$3" output_file="$4" log_file="$5"
  local champion history journal_excerpt
  initialize_journal
  champion="$(short_commit "$(get_state champion_commit)")"
  history="$(tail -n 31 "$RESULTS_FILE")"
  journal_excerpt="$(tail -n 500 "$JOURNAL_FILE")"
  {
    printf 'Read and follow autoresearch/program.md exactly.\n\n'
    printf 'This is experiment attempt %s. The current champion is %s.\n' "$attempt" "$champion"
    printf 'The trusted controller will evaluate after you return. Do not commit.\n\n'
    printf 'The complete controller-owned experiment journal is mirrored at '
    printf '`.heic-autoresearch/experiments.md`. Read it completely before choosing an idea.\n\n'
    printf 'Recent experiment ledger (TSV):\n%s\n' "$history"
    printf '\nRecent journal excerpt (Markdown):\n%s\n' "$journal_excerpt"
  } > "$prompt_file"

  local args=(exec --ephemeral --color never --sandbox workspace-write --cd "$ROOT_DIR" --output-last-message "$output_file")
  [[ -n "$model" ]] && args+=(--model "$model")
  log agent "Starting attempt $attempt"
  codex "${args[@]}" - < "$prompt_file" > "$log_file" 2>&1
}

cmd_run() {
  local hours="" max_experiments=0 model="" experiments=0
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --hours) hours="$2"; shift 2 ;;
      --max-experiments) max_experiments="$2"; shift 2 ;;
      --model) model="$2"; shift 2 ;;
      -h|--help) usage; return 0 ;;
      *) die "Unknown run option: $1" ;;
    esac
  done
  [[ -n "$hours" ]] || die "run requires --hours N"
  awk -v hours="$hours" 'BEGIN {exit(hours > 0 ? 0 : 1)}' || die "--hours must be positive."
  [[ "$max_experiments" =~ ^[0-9]+$ ]] || die "--max-experiments must be a non-negative integer."

  require_cmd codex
  ensure_external_state_dir
  initialize_journal
  require_champion_head
  require_clean_worktree
  check_environment_matches_setup
  verify_asset_integrity
  verify_champion_binary
  load_benchmark_files

  local lock_dir="$STATE_DIR/run.lock"
  acquire_run_lock "$lock_dir"
  trap 'exit 130' INT TERM
  rm -f "$STOP_FILE"

  local started deadline now attempt attempt_dir agent_status description eval_status
  started="$(date +%s)"
  deadline="$(awk -v start="$started" -v hours="$hours" 'BEGIN {printf "%.0f", start + hours * 3600}')"
  log run "Running for up to ${hours}h on $(current_branch); Ctrl-C leaves the current candidate for inspection."

  while :; do
    now="$(date +%s)"
    [[ "$now" -lt "$deadline" ]] || break
    [[ ! -e "$STOP_FILE" ]] || { log run "Stop requested."; break; }
    if [[ "$max_experiments" -gt 0 && "$experiments" -ge "$max_experiments" ]]; then
      break
    fi
    require_champion_head
    require_clean_worktree
    verify_asset_integrity

    attempt="$(get_state next_attempt)"
    set_state next_attempt "$((attempt + 1))"
    attempt_dir="$STATE_DIR/attempts/$attempt"
    mkdir -p "$attempt_dir"
    set +e
    run_agent_attempt "$attempt" "$model" "$attempt_dir/prompt.txt" \
      "$attempt_dir/agent-last.txt" "$attempt_dir/agent.log"
    agent_status=$?
    set -e
    # The repository copy is deliberately untrusted and ignored. Replace it
    # after every agent turn from the controller-owned external journal.
    mirror_journal
    description="$(first_nonempty_line "$attempt_dir/agent-last.txt")"
    description="${description:-agent attempt $attempt}"
    collect_changed_files
    if [[ "$agent_status" -ne 0 ]]; then
      if [[ ${#CHANGED_FILES[@]} -gt 0 ]]; then
        reject_candidate "$attempt" "$description" "Codex exited with status $agent_status" || true
      else
        record_result "$attempt" - - - - - "$(get_state cumulative_speedup)" \
          crash "$description" "Codex exited with status $agent_status"
      fi
      experiments=$((experiments + 1))
      continue
    fi

    set +e
    evaluate_candidate "$attempt" "$description"
    eval_status=$?
    set -e
    if [[ "$eval_status" -eq 1 ]]; then
      die "A trusted evaluation invariant failed; stopping the loop."
    fi
    experiments=$((experiments + 1))
  done

  log run "Finished $experiments attempt(s). Champion=$(short_commit "$(get_state champion_commit)") cumulative=$(get_state cumulative_speedup)x"
  log run "Results: $RESULTS_FILE"
}

cmd_evaluate() {
  local description="manual candidate"
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --description) description="$2"; shift 2 ;;
      -h|--help) usage; return 0 ;;
      *) die "Unknown evaluate option: $1" ;;
    esac
  done
  ensure_external_state_dir
  initialize_journal
  require_champion_head
  check_environment_matches_setup
  verify_asset_integrity
  load_benchmark_files
  local attempt
  attempt="$(get_state next_attempt)"
  set_state next_attempt "$((attempt + 1))"
  evaluate_candidate "$attempt" "$description"
}

cmd_bench() {
  local samples=5 temp_dir binary score
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --samples) samples="$2"; shift 2 ;;
      -h|--help) usage; return 0 ;;
      *) die "Unknown bench option: $1" ;;
    esac
  done
  [[ "$samples" =~ ^[1-9][0-9]*$ ]] || die "--samples must be a positive integer."
  ensure_external_state_dir
  load_benchmark_files
  temp_dir="$STATE_DIR/manual-benchmark"
  mkdir -p "$temp_dir"
  binary="$temp_dir/current-bench"
  build_benchmark_binary "$binary" "$temp_dir/build.log" || die "Benchmark build failed."
  score="$(benchmark_score "$binary" "$temp_dir/run.log" 1 "$samples" primary)" || die "Benchmark failed."
  cat "$temp_dir/run.log"
  log bench "score=${score}ms"
}

cmd_status() {
  ensure_external_state_dir
  [[ -f "$STATE_FILE" ]] || die "No baseline has been set up."
  initialize_journal
  printf 'branch:              %s\n' "$(get_state branch)"
  printf 'champion:            %s\n' "$(short_commit "$(get_state champion_commit)")"
  printf 'initial_score_ms:    %s\n' "$(get_state initial_score_ms)"
  printf 'champion_score_ms:   %s\n' "$(get_state champion_score_ms)"
  printf 'initial_confirm_ms:  %s\n' "$(get_state initial_confirmation_score_ms)"
  printf 'champion_confirm_ms: %s\n' "$(get_state champion_confirmation_score_ms)"
  printf 'confirmation_files:  %s\n' "$(get_state confirmation_file_count)"
  printf 'cumulative_speedup:  %sx\n' "$(get_state cumulative_speedup)"
  printf 'trusted_state:       %s\n' "$STATE_DIR"
  printf 'experiment_journal:  %s\n' "$JOURNAL_MIRROR"
  printf '\nRecent results:\n'
  tail -n 11 "$RESULTS_FILE"
}

cmd_stop() {
  ensure_external_state_dir
  [[ -f "$STATE_FILE" ]] || die "No active autoresearch baseline."
  : > "$STOP_FILE"
  log stop "Stop requested. The loop will stop before its next attempt."
}

main() {
  local command="${1:-}"
  if [[ -z "$command" || "$command" == "-h" || "$command" == "--help" ]]; then
    usage
    return 0
  fi
  shift
  cd "$ROOT_DIR"
  case "$command" in
    setup) cmd_setup "$@" ;;
    run) cmd_run "$@" ;;
    evaluate) cmd_evaluate "$@" ;;
    bench) cmd_bench "$@" ;;
    status) cmd_status "$@" ;;
    stop) cmd_stop "$@" ;;
    *) die "Unknown command: $command" ;;
  esac
}

main "$@"
