# HEIC decoder autoresearch

This directory adapts Karpathy's `autoresearch` pattern to decoder optimization:

- `program.md` is the human-owned research policy.
- `benchmark.rs` and `benchmark-corpus.txt` are the fixed evaluator.
- `scripts/autoresearch.sh` is the trusted outer loop.
- decoder source and dependencies are the agent-owned experiment surface.

The controller never asks the optimization agent to decide whether its own work
passes. It builds the current champion and candidate as separate executables,
runs them in baseline/candidate/candidate/baseline order, and only runs the full
correctness suite for a candidate that is at least 5% faster. It then repeats a
larger A/B benchmark across every pinned hook-decodable HEIC/HEIF corpus file. A
candidate is committed only after both speed gates and correctness pass.

The performance metric exclusively uses Ente's production image-crate hook
shape. It registers the HEIC decoder with Ente's guardrails, then calls
`ImageReader::open`, `with_guessed_format`, `into_decoder`, `icc_profile`,
`Limits::reserve`, `set_limits`, and `DynamicImage::from_decoder`. It never calls
the decoder's direct RGBA API. Consequently, an optimization that benefits only
the direct API cannot pass the performance gate.

## Validator policy

Keep pixel-exact libheif comparison for this phase. libheif is an output oracle,
not an implementation constraint: its speed and internal algorithms do not
limit how the Rust decoder is implemented. A tolerance would make regressions
harder to distinguish from deliberate numerical differences and should only be
introduced with a separate, independently justified quality metric and corpus.

The native iOS decoder is valuable as a performance target and a later secondary
cross-check, but it is not a portable ground-truth oracle. Platform color
management and rounding can differ, and requiring an attached phone would make
the core loop slower and less reproducible.

## Prerequisites

Set up the full harness from `TESTING.md` first. In particular, the libheif
checkout/binary, Ente fixtures, and generated stress corpus must exist. The six
real-camera files listed in `benchmark-corpus.txt` are required.

The loop calls the installed `codex` CLI. It defaults to that CLI's configured
model and authentication. The controller stores trusted binaries, hashes,
results, rejected patches, and logs outside the repository under
`~/.cache/heic-decoder-autoresearch/<repo-id>/`. This prevents a workspace-only
optimization agent from modifying the acceptance state.

Every terminal attempt is also appended to a controller-owned Markdown journal
at `~/.cache/heic-decoder-autoresearch/<repo-id>.experiments.md`. Its readable
repository mirror is `.heic-autoresearch/experiments.md`. The journal survives
fresh baselines and records the exact primary speed factor for every measured
success or failure, the full-corpus factor when that gate is reached, the
controller's rejection reason, a short learning, and the agent's report. The
controller refreshes the ignored mirror from trusted external state before and
after each agent turn, and every agent must read the complete journal before
choosing an experiment.

## Start a run

First commit the harness itself on `faster`, then begin from a clean worktree:

```bash
git status --short
scripts/autoresearch.sh setup
scripts/autoresearch.sh run --hours 8
```

Select a model explicitly if desired:

```bash
scripts/autoresearch.sh run --hours 8 --model <codex-model>
```

Useful controls from another terminal:

```bash
scripts/autoresearch.sh status
scripts/autoresearch.sh stop
less .heic-autoresearch/experiments.md
```

`stop` is cooperative: it stops before the next agent attempt, not in the middle
of an evaluation. To clear an old stop request, start `run` again.
Likewise, the wall-clock deadline prevents a new attempt from starting; an agent
or trusted evaluation already in flight is allowed to finish, so the final
elapsed time can exceed the requested budget.

To evaluate a candidate you edited manually, leave the changes uncommitted and
run:

```bash
scripts/autoresearch.sh evaluate --description "avoid coefficient copy"
```

## Acceptance gate

For each candidate the controller:

1. rejects changes outside `src/**/*.rs`, `Cargo.toml`, and `Cargo.lock`;
2. runs diff checks, `cargo fmt --check`, and the Rust test suite;
3. rejects dependency graphs containing native `links` packages (with a narrow
   exception for crates.io `rayon-core`, whose marker does not link native code);
4. builds a fresh candidate benchmark executable in trusted external state;
5. compares it against the saved champion with multiple interleaved samples on
   six large real-camera inputs;
6. requires at least `HEIC_AUTORESEARCH_MIN_IMPROVEMENT` (default `0.05`, or
   5%) improvement;
7. for a faster candidate, runs host clippy, installed portability target checks,
   and the complete pixel-exact validator + production-shaped image-hook suite;
8. after correctness, runs a second A/B benchmark over every HEIC/HEIF file that
   the baseline champion decoded through the hook during setup, requiring
   `HEIC_AUTORESEARCH_CONFIRM_MIN_IMPROVEMENT` (also 5% by default); and
9. commits the candidate and promotes its executable only if every check passes.

The primary benchmark measures Ente's path-based image-hook flow, including file
open/read, lazy layout probing, ICC retrieval, image limits, caller-buffer pixel
decode, and output allocation/deallocation. It checks full pixel and ICC
fingerprints outside the timed region. Lower `score_ms` is better. The broader
confirmation corpus is discovered once by the trusted baseline and stored
outside the agent-writable repository.

Rejected diffs are archived for review, but are removed from the worktree. The
Markdown journal records both accepted and rejected experiments so later agents
do not repeat an unsuccessful implementation without addressing its measured
result or failure. The loop refuses to start unless the branch, HEAD, and
tracked/untracked worktree match the saved champion, so unrelated user changes
are never discarded.

## Portability and final promotion

One autoresearch run optimizes the machine it runs on; it cannot establish a
4–7x speedup on every architecture. Repeat the benchmark/loop or at least the
final A/B benchmark on representative ARM and x86-64 hosts. Before opening the
PR, run the normal CI-equivalent commands from `TESTING.md` and review all kept
commits for maintainability, safety, dependency quality, and over-specialization.

Environment knobs:

- `HEIC_AUTORESEARCH_MIN_IMPROVEMENT=0.10` requires a 10% primary win.
- `HEIC_AUTORESEARCH_CONFIRM_MIN_IMPROVEMENT=0.10` requires a 10% full-corpus
  confirmation.
- `HEIC_AUTORESEARCH_PAIR_SAMPLES=3` increases A/B samples per invocation.
- `HEIC_AUTORESEARCH_CONFIRM_SAMPLES=4` increases full-corpus A/B samples.
- `HEIC_AUTORESEARCH_CHECK_TARGETS=aarch64-apple-ios,aarch64-linux-android`
  selects extra installed targets checked before promotion.
- `HEIC_AUTORESEARCH_STATE_DIR=/trusted/path` overrides external state.
- Existing `HEIC_LIBHEIF_SOURCE_DIR`, `HEIC_ENTE_FIXTURES_DIR`,
  `LIBHEIF_BUILD_DIR`, and `LIBHEIF_DEC_BIN` overrides are captured by `setup`.

If the validator, corpus, benchmark, controller, toolchain, compiler flags, or
machine changes, start a fresh baseline with `scripts/autoresearch.sh setup`.
