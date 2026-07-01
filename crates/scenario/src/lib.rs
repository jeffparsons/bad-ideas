//! Shared, implementation-agnostic definition of the benchmark scenario.
//!
//! Every implementation of the simulation (the boring native baseline, and later
//! the Wasm Component / Wasm GC variants) depends on this crate for three things:
//!
//! 1. The fixed scenario parameters ([`ENTITY_COUNT`], [`STEPS`], [`DT`], [`SEED`]).
//! 2. A deterministic [`initial_state`] so every implementation starts from a
//!    byte-identical set of entities and can be cross-checked for correctness.
//! 3. A [`checksum`] fingerprint of the final state plus a timing [`measure`]
//!    harness that emits machine-readable JSON for the eventual charts.
//!
//! This crate deliberately does *not* impose a memory layout: it hands out raw
//! `[f32; 4]` rows (`[px, py, vx, vy]`) and validates raw `[f32; 4]` rows, leaving
//! each implementation free to use array-of-structs, struct-of-arrays, Wasm-side
//! buffers, or whatever else it likes.

use std::hint::black_box;
use std::time::{Duration, Instant};

/// Number of independent entities in the simulation.
pub const ENTITY_COUNT: usize = 10_000;
/// Number of integration steps to compute and measure.
pub const STEPS: usize = 1000;
/// Integration timestep, in seconds.
pub const DT: f32 = 0.01;
/// Fixed seed for the initial-state PRNG. Chosen arbitrarily; the only requirement
/// is that it never changes, so results stay comparable across implementations.
pub const SEED: u64 = 0x1234_5678_9ABC_DEF0;

/// A minimal, portable SplitMix64 PRNG.
///
/// Hand-rolled (rather than pulling in `rand`) so the exact bit sequence is
/// reproducible everywhere, including inside a Wasm guest — the initial state must
/// be identical across implementations for cross-checking to mean anything.
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    /// Create a new generator seeded with `seed`.
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// Advance the generator and return the next 64-bit output.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Return the next `f32` uniformly distributed in `[-1.0, 1.0)`.
    ///
    /// Uses the top 24 bits of the raw output (the full mantissa precision of an
    /// `f32`) to build a value in `[0, 1)`, then maps it onto `[-1, 1)`.
    pub fn next_f32_signed(&mut self) -> f32 {
        let bits = (self.next_u64() >> 40) as u32; // top 24 bits
        let unit = (bits as f32) / ((1u32 << 24) as f32); // [0, 1)
        unit * 2.0 - 1.0 // [-1, 1)
    }
}

/// Generate the deterministic initial state: `ENTITY_COUNT` rows of
/// `[px, py, vx, vy]`, each component drawn from `[-1.0, 1.0)` in that fixed order.
pub fn initial_state() -> Vec<[f32; 4]> {
    let mut rng = SplitMix64::new(SEED);
    let mut entities = Vec::with_capacity(ENTITY_COUNT);
    for _ in 0..ENTITY_COUNT {
        let px = rng.next_f32_signed();
        let py = rng.next_f32_signed();
        let vx = rng.next_f32_signed();
        let vy = rng.next_f32_signed();
        entities.push([px, py, vx, vy]);
    }
    entities
}

/// Deterministic fingerprint of a (typically final) state.
///
/// A canonical fold over the state in a fixed order (entity `0..N`, then
/// `px, py, vx, vy`), accumulated in `f64` to avoid losing the whole sum to `f32`
/// rounding. Two implementations that perform identical `f32` arithmetic produce
/// bit-identical states and therefore an identical checksum, so this doubles as a
/// cheap cross-implementation correctness check.
pub fn checksum(state: &[[f32; 4]]) -> f64 {
    let mut acc = 0.0f64;
    for row in state {
        for &c in row {
            acc += c as f64;
        }
    }
    acc
}

/// Timing results for one implementation, ready to serialize for the charts.
pub struct Results {
    /// Implementation name (also the results filename stem).
    pub name: String,
    pub entity_count: usize,
    pub steps: usize,
    pub warmups: usize,
    pub repeats: usize,
    /// Wall-clock duration of each timed repeat, in nanoseconds.
    pub durations_ns: Vec<u128>,
    /// Fingerprint of the final state (identical across all repeats).
    pub checksum: f64,
}

impl Results {
    /// Fastest observed repeat, in nanoseconds (best-case throughput).
    pub fn min_ns(&self) -> u128 {
        self.durations_ns.iter().copied().min().unwrap_or(0)
    }

    /// Mean repeat duration, in nanoseconds.
    pub fn mean_ns(&self) -> f64 {
        if self.durations_ns.is_empty() {
            return 0.0;
        }
        let sum: u128 = self.durations_ns.iter().sum();
        sum as f64 / self.durations_ns.len() as f64
    }

    /// Peak throughput in entity-steps per second, derived from the fastest repeat.
    /// One "entity-step" is a single entity advanced by one integration step.
    pub fn entity_steps_per_sec(&self) -> f64 {
        let min = self.min_ns();
        if min == 0 {
            return 0.0;
        }
        let entity_steps = (self.entity_count as f64) * (self.steps as f64);
        entity_steps / (min as f64 / 1e9)
    }

    /// Serialize to a compact, hand-written JSON document. The shape is fixed and
    /// tiny, so this avoids a serde dependency. `f64` fields use Rust's default
    /// `Display`, which prints the shortest round-trippable representation.
    pub fn to_json(&self) -> String {
        let durs: Vec<String> = self.durations_ns.iter().map(|d| d.to_string()).collect();
        format!(
            "{{\n  \"name\": \"{}\",\n  \"entity_count\": {},\n  \"steps\": {},\n  \"warmups\": {},\n  \"repeats\": {},\n  \"durations_ns\": [{}],\n  \"min_ns\": {},\n  \"mean_ns\": {},\n  \"entity_steps_per_sec\": {},\n  \"checksum\": {}\n}}\n",
            self.name,
            self.entity_count,
            self.steps,
            self.warmups,
            self.repeats,
            durs.join(", "),
            self.min_ns(),
            self.mean_ns(),
            self.entity_steps_per_sec(),
            self.checksum,
        )
    }

    /// Write the JSON document to `results/<name>.json`, relative to the current
    /// working directory (the workspace root when run via `cargo run`). Returns the
    /// path written.
    pub fn write_json(&self) -> std::io::Result<std::path::PathBuf> {
        let dir = std::path::Path::new("results");
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("{}.json", self.name));
        std::fs::write(&path, self.to_json())?;
        Ok(path)
    }
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// Run and time an implementation.
///
/// `run` performs one full simulation (build initial state, compute all [`STEPS`]
/// steps) and returns the [`checksum`] of the final state. It is invoked
/// `BENCH_WARMUPS` times untimed (default 2) to warm caches and let the CPU ramp,
/// then `BENCH_REPEATS` times under a wall-clock timer (default 5). All repeats
/// must agree on the checksum — a mismatch means the simulation is not
/// deterministic and is treated as a bug.
pub fn measure<F: FnMut() -> f64>(name: &str, mut run: F) -> Results {
    let warmups = env_usize("BENCH_WARMUPS", 2);
    let repeats = env_usize("BENCH_REPEATS", 5).max(1);

    for _ in 0..warmups {
        black_box(run());
    }

    let mut durations_ns = Vec::with_capacity(repeats);
    let mut checksums = Vec::with_capacity(repeats);
    for _ in 0..repeats {
        let start = Instant::now();
        let c = run();
        let elapsed: Duration = start.elapsed();
        black_box(c);
        durations_ns.push(elapsed.as_nanos());
        checksums.push(c);
    }

    for c in &checksums {
        assert_eq!(
            *c, checksums[0],
            "non-deterministic checksum across repeats for '{name}'"
        );
    }

    Results {
        name: name.to_string(),
        entity_count: ENTITY_COUNT,
        steps: STEPS,
        warmups,
        repeats,
        durations_ns,
        checksum: checksums[0],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_state_is_deterministic() {
        assert_eq!(initial_state(), initial_state());
    }

    #[test]
    fn initial_state_has_expected_shape() {
        let state = initial_state();
        assert_eq!(state.len(), ENTITY_COUNT);
    }

    #[test]
    fn prng_output_is_in_signed_unit_range() {
        let mut rng = SplitMix64::new(SEED);
        for _ in 0..100_000 {
            let x = rng.next_f32_signed();
            assert!((-1.0..1.0).contains(&x), "out of range: {x}");
        }
    }

    #[test]
    fn checksum_matches_hand_computation() {
        let state = [[1.0, 2.0, 3.0, 4.0], [-1.0, -2.0, -3.0, -4.0]];
        assert_eq!(checksum(&state), 0.0);
    }
}
