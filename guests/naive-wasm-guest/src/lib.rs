//! Guest for issue #2: the naive Wasm Component.
//!
//! Exports a single `advance` function that integrates *one* entity by one step.
//! The host calls it once per entity per step, so the per-call boundary cost is the
//! whole story here. The math is identical to `crates/baseline/src/main.rs::step`,
//! so the final state (and its checksum) should match the native baseline exactly.

wit_bindgen::generate!({
    world: "guest",
    path: "wit",
});

use exports::bad_ideas::naive::sim::{Entity, Guest};

struct Component;

impl Guest for Component {
    fn advance(e: Entity, dt: f32) -> Entity {
        // Acceleration is a unit vector toward the origin (magnitude 1.0/sec);
        // zero at the origin. Semi-implicit Euler: v += a*dt, then p += v*dt.
        let len = (e.px * e.px + e.py * e.py).sqrt();
        let (ax, ay) = if len > 0.0 {
            (-e.px / len, -e.py / len)
        } else {
            (0.0, 0.0)
        };
        let vx = e.vx + ax * dt;
        let vy = e.vy + ay * dt;
        Entity {
            px: e.px + vx * dt,
            py: e.py + vy * dt,
            vx,
            vy,
        }
    }
}

export!(Component);
