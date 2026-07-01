# bad-ideas

> **AI use disclosure:** the implementations, tooling, and much of this README are
> largely written by [Claude Code](https://www.anthropic.com/claude-code), under
> human direction and review.

**How fast can ECS go when the "systems" run in [Wasm Components](https://component-model.bytecodealliance.org/)?**

Wasm Components are the thing I actually want here — for the ergonomics and the
use cases they unlock (portable, sandboxed, language-agnostic systems you can mix
and match at runtime). The going-in assumption is that this will be *pretty damn
slow* next to an everything-native approach; the boundary-crossing overhead is
real and inherent. This experiment isn't about pretending that cost away — it's
about seeing **how good we can make it anyway**: which tricks close the gap, how
far they get us, and exactly where the remaining limits come from.

So we start with a boring native baseline to measure against, then push on the
Wasm Component approaches (with side quests like **Wasm GC**) and see how close we
can get.

Every implementation runs the *same* fixed scenario, from the *same* deterministic
initial state, so the numbers are directly comparable and correctness can be
cross-checked via a shared checksum.

## The scenario

- 100,000 independent entities in 2D.
- Each entity stores position and velocity (`2×f32` each); acceleration is derived.
- Deterministic initial state: every float seeded from a fixed PRNG in `[-1, 1)`.
- Integration step (`dt = 0.01s`, semi-implicit Euler): accelerate toward the
  origin at 1 unit/s (`a = -normalize(pos)`), then `v += a·dt`, then `p += v·dt`.
- Compute and measure 1000 steps.

The scenario parameters, deterministic initial state, correctness checksum, and
timing harness all live in the shared [`scenario`](crates/scenario) crate. Each
implementation is a separate crate that plugs into it.

## Implementations

| Issue | Crate | Description | Status |
| --- | --- | --- | --- |
| [#1](https://github.com/jeffparsons/bad-ideas/issues/1) | [`baseline`](crates/baseline) | Array-of-structs native Rust reference | ✅ |
| [#2](https://github.com/jeffparsons/bad-ideas/issues/2) | [`naive-wasm`](crates/naive-wasm) | Naive Wasm Component (host call per entity per step) | ✅ |
| [#3](https://github.com/jeffparsons/bad-ideas/issues/3) | [`stream-wasm`](crates/stream-wasm) | Wasm Component with streamed inputs/outputs (one async `stream<entity>` call per step) | ✅ |
| [#3†](https://github.com/jeffparsons/bad-ideas/issues/3) | [`bulk-wasm`](crates/bulk-wasm) | Bulk comparison: one `list<entity>` call per step (synchronous, no streaming) | ✅ |

## Results

<!-- BENCH_ENV:START -->
_Measured on:_

- **CPU:** Apple M1 Pro
- **OS / arch:** macos / aarch64
- **Toolchain:** rustc 1.96.0 (ac68faa20 2026-05-25)
- **Build profile:** release (`opt-level=3`, `lto=true`, `codegen-units=1`)
- **Warmup / timed repeats:** 2 / 5
<!-- BENCH_ENV:END -->

<!-- BENCH_TABLE:START -->
| Implementation | Entities | Steps | Fastest | Mean | Throughput | Checksum |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Array-of-structs native reference (`baseline`) | 100,000 | 1,000 | 83.65 ms | 83.88 ms | 1195.4 M/s | -69.2070479314134 |
| Naive Wasm Component (host call per entity per step) (`naive-wasm`) | 100,000 | 1,000 | 30458.82 ms | 30466.00 ms | 3.3 M/s | -69.2070479314134 |
| Streaming Wasm Component (one async stream<entity> call per step) (`stream-wasm`) | 100,000 | 1,000 | 2652.32 ms | 2841.82 ms | 37.7 M/s | -69.2070479314134 |
| Bulk Wasm Component (one list<entity> call per step) (`bulk-wasm`) | 100,000 | 1,000 | 1440.39 ms | 1462.27 ms | 69.4 M/s | -69.2070479314134 |

<!-- BENCH_TABLE:END -->

> _Throughput_ is entity-steps per second (entities × steps ÷ fastest run).
> A matching _checksum_ means two implementations computed the same final state.
> These numbers are machine-specific — see the environment block above for what
> produced the committed results.

## Regenerating the results

Build everything in release mode, run all benchmarks, and update the tables above:

```sh
./scripts/bench.sh
```

Use `./scripts/bench.sh --readme-only` to regenerate the tables from existing
`results/*.json` without rebuilding or re-running (handy when tweaking formatting).
Warmup and timed-repeat counts can be overridden with the `BENCH_WARMUPS` and
`BENCH_REPEATS` environment variables.
