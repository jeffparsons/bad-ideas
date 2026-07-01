//! Issue #1: the boring, non-Wasm, array-of-structs native reference implementation.
//!
//! This is the "how fast is this natively" baseline that every later Wasm
//! implementation is measured against. It is deliberately unclever: entities live
//! in a plain `Vec<Entity>` (array-of-structs) and the integration step is a single
//! scalar loop with no SIMD, threading, or other tricks.

use scenario::{checksum, initial_state, measure, DT, STEPS};

/// One entity: position and velocity stored adjacently (array-of-structs).
/// Acceleration is not stored; it is derived each step from the position.
#[derive(Clone, Copy)]
struct Entity {
    px: f32,
    py: f32,
    vx: f32,
    vy: f32,
}

/// Advance every entity by one integration step (semi-implicit Euler).
///
/// Acceleration is a unit vector pointing at the origin (magnitude 1.0 per second):
/// `a = -normalize(position)`. The degenerate case of an entity exactly at the
/// origin yields zero acceleration. Velocity is updated from acceleration, then
/// position is updated from the new velocity.
fn step(entities: &mut [Entity], dt: f32) {
    for e in entities.iter_mut() {
        let len = (e.px * e.px + e.py * e.py).sqrt();
        let (ax, ay) = if len > 0.0 {
            (-e.px / len, -e.py / len)
        } else {
            (0.0, 0.0)
        };
        e.vx += ax * dt;
        e.vy += ay * dt;
        e.px += e.vx * dt;
        e.py += e.vy * dt;
    }
}

/// Build a fresh simulation from the shared initial state, run all [`STEPS`] steps,
/// and return the final state as `[px, py, vx, vy]` rows for checksumming.
fn run() -> Vec<[f32; 4]> {
    let mut entities: Vec<Entity> = initial_state()
        .iter()
        .map(|&[px, py, vx, vy]| Entity { px, py, vx, vy })
        .collect();

    for _ in 0..STEPS {
        step(&mut entities, DT);
    }

    entities.iter().map(|e| [e.px, e.py, e.vx, e.vy]).collect()
}

fn main() {
    let results = measure("baseline", || checksum(&run()));

    let mean_ms = results.mean_ns() / 1e6;
    let min_ms = results.min_ns() as f64 / 1e6;
    let per_step_us = (results.min_ns() as f64 / results.steps as f64) / 1e3;

    println!("baseline: array-of-structs native reference");
    println!(
        "  {} entities x {} steps, {} warmup + {} timed repeats",
        results.entity_count, results.steps, results.warmups, results.repeats
    );
    println!("  fastest run: {min_ms:.2} ms  (mean {mean_ms:.2} ms)");
    println!("  per step:    {per_step_us:.2} us");
    println!(
        "  throughput:  {:.2} M entity-steps/sec",
        results.entity_steps_per_sec() / 1e6
    );
    println!("  checksum:    {}", results.checksum);

    match results.write_json() {
        Ok(path) => println!("wrote {}", path.display()),
        Err(e) => eprintln!("failed to write results: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_step_matches_hand_computation() {
        // Entity at (1, 0) with zero velocity. Acceleration = -normalize((1,0)) = (-1, 0).
        // After one step with dt = 0.01:
        //   vx = 0 + (-1) * 0.01      = -0.01
        //   px = 1 + (-0.01) * 0.01   =  0.9999
        // vy and py stay 0.
        let mut entities = [Entity {
            px: 1.0,
            py: 0.0,
            vx: 0.0,
            vy: 0.0,
        }];
        step(&mut entities, 0.01);

        let e = entities[0];
        let eps = 1e-6;
        assert!((e.vx - -0.01).abs() < eps, "vx = {}", e.vx);
        assert!(e.vy.abs() < eps, "vy = {}", e.vy);
        assert!((e.px - 0.9999).abs() < eps, "px = {}", e.px);
        assert!(e.py.abs() < eps, "py = {}", e.py);
    }

    #[test]
    fn run_is_deterministic() {
        assert_eq!(checksum(&run()), checksum(&run()));
    }
}
