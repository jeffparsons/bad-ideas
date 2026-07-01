//! Guest for issue #3 (bulk comparison variant).
//!
//! Exports a single `advance` that integrates a *whole batch* of entities per call.
//! The host calls it once per step (1000 calls total, vs the naive guest's 100M),
//! so this isolates the cost of one bulk `list<entity>` copy in and out per step —
//! the comparison point for the streaming variant. Same math as the baseline, so the
//! final state (and checksum) matches exactly.

wit_bindgen::generate!({
    world: "guest",
    path: "wit",
});

use exports::bad_ideas::bulk::sim::{Entity, Guest};

struct Component;

impl Guest for Component {
    fn advance(mut entities: Vec<Entity>, dt: f32) -> Vec<Entity> {
        for e in entities.iter_mut() {
            *e = step(*e, dt);
        }
        entities
    }
}

/// One semi-implicit Euler step: accelerate toward the origin at 1 unit/s
/// (`a = -normalize(pos)`, zero at the origin), then `v += a*dt`, then `p += v*dt`.
fn step(e: Entity, dt: f32) -> Entity {
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

export!(Component);
