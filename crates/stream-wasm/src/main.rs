//! Issue #3 (streaming variant): one *async* component call per step, streaming all
//! entities in and out via the Component Model's `stream<entity>` type.
//!
//! Per step the host: (1) creates an input stream from the current entity `Vec`
//! (the built-in `Vec` `StreamProducer` — a move, not a copy), (2) calls the guest's
//! async `advance` to get back an output stream, and (3) drains that output stream
//! into the next state via a bulk-collecting `StreamConsumer`. All of this happens
//! inside `Store::run_concurrent` so the guest's producer and the host's consumer run
//! concurrently. One `advance` call per step lowers to many core-wasm stream
//! read/write calls — the "single component call → many core calls" of the issue.

use std::pin::Pin;
use std::task::{Context, Poll};

use anyhow::Result;
use futures::channel::mpsc;
use futures::StreamExt;
use scenario::{checksum, initial_state, measure, DT, ENTITY_COUNT, STEPS};
use wasmtime::component::{Component, Linker, Source, StreamConsumer, StreamReader, StreamResult};
use wasmtime::{Config, Engine, Store, StoreContextMut};
use wasmtime_wasi::p2::add_to_linker_async;
use wasmtime_wasi::{ResourceTable, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

wasmtime::component::bindgen!({
    path: "../../guests/stream-wasm-guest/wit",
    world: "guest",
    exports: { default: async },
});

use exports::bad_ideas::streaming::sim::Entity;

const COMPONENT_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../target/wasm32-wasip2/release/stream_wasm_guest.wasm"
);

/// Store state: just enough WASI to satisfy the guest's `wasi:*` imports (the async
/// runtime pulls them in; none are exercised on the hot path).
struct Host {
    ctx: WasiCtx,
    table: ResourceTable,
}

impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

/// A `StreamConsumer` that drains each batch of items the guest writes into an
/// unbounded channel, so the host side can `collect()` the whole output stream.
struct Collector {
    tx: mpsc::UnboundedSender<Vec<Entity>>,
}

impl<D> StreamConsumer<D> for Collector {
    type Item = Entity;

    fn poll_consume(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        store: StoreContextMut<D>,
        mut source: Source<Self::Item>,
        _finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        // The runtime only calls this when at least one item is available. Drain the
        // whole batch (up to ENTITY_COUNT) in one go and forward it.
        let mut buf: Vec<Entity> = Vec::with_capacity(ENTITY_COUNT);
        source.read(store, &mut buf)?;
        let _ = self.get_mut().tx.unbounded_send(buf);
        Poll::Ready(Ok(StreamResult::Completed))
    }
}

fn build_engine() -> wasmtime::Result<Engine> {
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    Engine::new(&config)
}

/// Run one full simulation: fresh state, `STEPS` streamed steps, return final state.
async fn run(store: &mut Store<Host>, guest: &Guest) -> Result<Vec<[f32; 4]>> {
    store
        .run_concurrent(async |accessor| -> Result<Vec<[f32; 4]>> {
            let mut state: Vec<Entity> = initial_state()
                .iter()
                .map(|&[px, py, vx, vy]| Entity { px, py, vx, vy })
                .collect();

            for _ in 0..STEPS {
                let input =
                    accessor.with(|store| StreamReader::new(store, std::mem::take(&mut state)))?;
                let output = guest
                    .bad_ideas_streaming_sim()
                    .call_advance(accessor, input, DT)
                    .await?;

                let (tx, rx) = mpsc::unbounded();
                accessor.with(|store| output.pipe(store, Collector { tx }))?;
                let batches: Vec<Vec<Entity>> = rx.collect().await;
                state = batches.into_iter().flatten().collect();
            }

            Ok(state.iter().map(|e| [e.px, e.py, e.vx, e.vy]).collect())
        })
        .await?
}

fn main() -> Result<()> {
    let engine = build_engine()?;
    let component = Component::from_file(&engine, COMPONENT_PATH)?;
    let mut linker = Linker::new(&engine);
    add_to_linker_async(&mut linker)?;
    let host = Host {
        ctx: WasiCtxBuilder::new().build(),
        table: ResourceTable::new(),
    };
    let mut store = Store::new(&engine, host);
    let guest =
        futures::executor::block_on(Guest::instantiate_async(&mut store, &component, &linker))?;

    let results = measure("stream-wasm", || {
        let final_state = futures::executor::block_on(run(&mut store, &guest))
            .expect("stream simulation failed");
        checksum(&final_state)
    });

    let mean_ms = results.mean_ns() / 1e6;
    let min_ms = results.min_ns() as f64 / 1e6;
    let per_step_us = (results.min_ns() as f64 / results.steps as f64) / 1e3;

    println!("stream-wasm: one async stream<entity> call per step");
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

    let path = results.write_json()?;
    println!("wrote {}", path.display());
    Ok(())
}
