# Reemoth

Low-latency peer-to-peer remote desktop for Windows and Linux. See [PLAN.md](PLAN.md) for the design and milestones.

## Layout

- `crates/reemoth-proto`: wire types shared by the app and the signaling Worker (wasm-compatible)
- `apps/reemoth`: the app binary
- `signaling/`: Cloudflare Worker + Durable Objects for signaling (separate Cargo workspace, `wasm32-unknown-unknown`)

## Develop

Prerequisites: Rust stable, the `wasm32-unknown-unknown` target, Node 22+, and `cargo install worker-build --version 0.8.7 --locked`.

```bash
cargo test --workspace
```

Run the signaling Worker locally (no Cloudflare account needed):

```bash
cd signaling && npm ci && npx wrangler dev --port 8787
```

Ping it from the app:

```bash
cargo run -p reemoth -- signal-ping --server ws://127.0.0.1:8787
```

Deploy with `npx wrangler deploy` from `signaling/` (requires `wrangler login`).
