# Reemoth

Low-latency peer-to-peer remote desktop for Windows and Linux. See [PLAN.md](PLAN.md) for the design and milestones.

**Status: M1 (connectivity).** Two PCs on different networks find each other by host ID, authenticate with the host's password, and open a direct encrypted UDP link with no port forwarding. A TURN relay is used when a direct path is impossible. There is no screen sharing yet; `connect` measures latency and throughput over the link and opens a text chat to the host.

## Layout

- `crates/reemoth-proto`: wire types shared by the app and the signaling Worker (wasm-compatible)
- `crates/reemoth-auth`: host identity (Ed25519), password PAKE (SPAKE2 + Argon2id), session keys, lockout
- `crates/reemoth-net`: STUN/TURN client, candidate gathering, WebRTC session loop (str0m)
- `apps/reemoth`: the CLI (`host`, `connect`, `set-password`, `info`, `signal-ping`)
- `signaling/`: Cloudflare Worker + Durable Objects for signaling (separate Cargo workspace, `wasm32-unknown-unknown`)

## Testing between two PCs

### 1. Deploy the signaling server (once)

Both PCs need to reach it, so it runs on your Cloudflare account (the free plan is enough):

```bash
cd signaling && npm ci && npx wrangler login && npx wrangler deploy
```

`wrangler deploy` prints a URL like `https://reemoth-signaling.<you>.workers.dev`; pass it to `--signal` as is.

Optional, for networks where a direct connection is impossible (both sides behind strict or carrier-grade NAT): create a TURN key under Cloudflare dashboard → Realtime → TURN, then:

```bash
npx wrangler secret put TURN_KEY_ID
npx wrangler secret put TURN_KEY_API_TOKEN
```

### 2. Get the binary on each PC

- Download `reemoth-windows-x86_64` / `reemoth-linux-x86_64` from the latest CI run's artifacts (the Linux build is static and runs on any x86_64 distro; `chmod +x reemoth` after unzipping), or
- Build it: `cargo build --release -p reemoth` (binary in `target/release/`).

### 3. Run

On the PC to be controlled (the host):

```bash
reemoth host --signal wss://reemoth-signaling.<you>.workers.dev
```

The first run asks for a password and prints the host's ID. The signaling URL is saved, so later runs need only `reemoth host`. If Windows Firewall asks, allow access.

On the other PC:

```bash
reemoth connect 123456789 --signal wss://reemoth-signaling.<you>.workers.dev
```

It prints which path was used (`direct (local network)`, `direct (peer-to-peer over the internet)`, or `relayed`), the round-trip latency, and the throughput, then lets you type messages that appear on the host. Add `--relay-only` to force the TURN path, and `-v` for detailed logs.

`reemoth info` shows the saved ID and settings.

## Develop

Prerequisites: Rust stable, the `wasm32-unknown-unknown` target, Node 22+, and `cargo install worker-build --version 0.8.7 --locked`.

```bash
cargo test --workspace
```

Run the signaling Worker locally (no Cloudflare account needed):

```bash
cd signaling && npm ci && npx wrangler dev --port 8787
```

Full end-to-end check against it (host, wrong password, connect, latency, throughput):

```bash
cargo build -p reemoth && bash scripts/e2e.sh target/debug/reemoth ws://127.0.0.1:8787
```
