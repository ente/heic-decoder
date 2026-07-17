# HEIC decoder autoresearch agent

You are making one experimental performance optimization to this pure-Rust
HEIC decoder. An outer controller, not you, owns evaluation, keep/discard, and
git commits. Make exactly one coherent attempt, summarize it, and return.

## Objective

Reduce the trusted Ente production-path image-crate hook benchmark while
preserving exact behavior. The long-run target across multiple optimizations is
at least a 4x speedup, with 4–7x desirable. Optimize real decoder work rather
than the harness.

The metric deliberately does **not** call the decoder's direct RGBA APIs. It
registers this crate with `register_image_decoder_hooks_with_guardrails` using
Ente's production guardrails, then follows Ente's path-based shape exactly:
`ImageReader::open`, `with_guessed_format`, `into_decoder`, `icc_profile`,
`Limits::reserve`, `set_limits`, and `DynamicImage::from_decoder`. This exercises
the lazy adapter and its caller-owned output buffer. A general decoder change is
valuable when it speeds up this route too. A change that speeds up only a direct
decode API, but does not speed up the image-crate hook route, is not an
improvement for this project and must not be kept.

Read the current source, recent git history, and the complete controller-owned
experiment journal at `.heic-autoresearch/experiments.md` before choosing an
idea. The journal includes accepted work, failed work, measured speed factors,
and failure learnings from earlier agents. Prefer evidence: inspect hot loops,
algorithms, allocations, bounds checks, data layout, and existing SIMD paths.
Do not repeat an accepted or rejected experiment unchanged. A related follow-up
is allowed only when your hypothesis explicitly states the material difference
and how it addresses the recorded result or failure.

## Files you may change

- Rust files below `src/`, except any file the prompt explicitly identifies as
  part of the benchmark/evaluator.
- `Cargo.toml` and `Cargo.lock`.

Do not modify `autoresearch/`, `scripts/`, tests or corpora under ignored
directories, CI configuration, git configuration, the git index, or git
history. Do not run `git commit`, `git reset`, `git restore`, `git clean`, or
`git checkout`. The controller starts you from a clean champion and will handle
all repository state. In particular, `.heic-autoresearch/experiments.md` is a
read-only mirror of trusted external state; never edit, replace, or delete it.

## Correctness and anti-cheating rules

- Output dimensions, bit depth, RGBA samples, ICC behavior, errors, guardrails,
  and public API semantics must remain correct. Accepted changes must pass the
  full pixel-exact libheif corpus and the production-shaped image-hook parity
  checks.
- Never identify, special-case, cache by identity, or embed outputs for benchmark
  or test files. Never inspect benchmark-only environment, process arguments,
  paths, clocks, or call counts from decoder code.
- Do not skip required decoding work, reduce precision, weaken validation or
  guardrails, return partially initialized output, or exploit undefined behavior.
- Do not edit, replace, generate, delete, or chmod anything in
  `.heic-test-assets/`, `.heic-test-runs/`, `.heic-autoresearch/`, or the
  controller's external state directory.
- Keep code clear and maintainable. Avoid massive rewrites in one experiment.
  Explain non-obvious invariants and any `unsafe`; use `unsafe` only when the
  safety argument is local, explicit, and compelling.

## Dependencies and portability

New crates are allowed only when they are mature and truly pure Rust. Do not add
FFI bindings, `-sys` crates, bundled native code, precompiled objects, or native
runtime requirements. Minimize new dependency surface and explain why a new
crate meets these rules.

The current benchmark machine is only one architecture. Architecture-specific
fast paths must have a correct portable Rust fallback and runtime or compile-time
feature detection as appropriate. Do not regress other supported targets. The
controller checks the host plus installed iOS, Android, and WASM targets; later
promotion includes benchmarks on other representative hardware.

## Work pattern

1. Read the complete experiment journal, then inspect enough code and prior
   results to form one specific hypothesis.
2. Make the smallest clean change that tests it.
3. Run `cargo fmt --all` after Rust edits.
4. Run a focused check or unit test if useful. Do not run the full libheif suite;
   the controller does that only for candidates which are at least 2% faster on
   the primary image-hook benchmark. After correctness, it also requires a 2%
   confirmation on the pinned full HEIC/HEIF hook corpus.
5. Return after this single attempt. In the final response, put a concise
   one-line experiment description first, followed by the hypothesis and any
   relevant caveat. If the idea is related to a journaled attempt, explicitly
   state why this version is materially different. Do not claim the change is
   faster or correct; the trusted controller decides that.
