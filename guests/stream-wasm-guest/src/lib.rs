//! Guest for issue #3 (streaming variant).
//!
//! Exports an *async* `advance` that consumes an input `stream<entity>` and produces
//! an output `stream<entity>`. The host calls it once per step (1000 calls total),
//! streaming all 100k entities in and out per call — which lowers to many core-wasm
//! stream read/write operations under one component call.
//!
//! `advance` returns the output stream's read end immediately and `spawn_local`s a
//! producer task (driven by wit-bindgen's built-in component-model async reactor)
//! that reads the input, integrates it, and writes the output. Spawning is required
//! because the producer must run *concurrently* with the host draining the output.
//! Same math as the baseline, so the final state (and checksum) matches exactly.

wit_bindgen::generate!({
    world: "guest",
    path: "wit",
    async: true,
});

use exports::bad_ideas::streaming::sim::{Entity, Guest};
use wit_bindgen::spawn_local;
use wit_bindgen::StreamReader;

struct Component;

impl Guest for Component {
    async fn advance(input: StreamReader<Entity>, dt: f32) -> StreamReader<Entity> {
        let (mut tx, rx) = wit_stream::new();
        spawn_local(async move {
            let mut items = input.collect().await;
            for e in items.iter_mut() {
                *e = step(*e, dt);
            }
            // Write everything back; dropping `tx` afterwards ends the output stream.
            let _ = tx.write_all(items).await;
        });
        rx
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
