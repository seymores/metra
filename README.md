# Metra

Open-source (Apache-2.0) large-file transfer platform designed to rival Aspera using a QUIC-first data plane and REST control plane.

## Current Vertical Slice

- Rust workspace with three crates:
  - `metra-proto`: shared API and transfer models.
  - `metra-server`: REST control plane + QUIC upload receiver.
  - `metra-client`: TUI-first client with scriptable CLI commands and QUIC sender.
- REST endpoints:
  - `GET /health`
  - `GET /v1/quic/certificate`
  - `POST /v1/transfers`
  - `GET /v1/transfers/{transfer_id}`
- v1 transfer request validation includes 1 MiB resume chunk sizing.
- OpenTelemetry-enabled tracing pipeline initialized in server.
- Resumable upload path:
  - client sends QUIC transfer-open control frame
  - server replies with resume offset based on staged bytes
  - client streams remaining bytes only

## Architecture Direction

- Control plane: REST.
- Data plane: custom QUIC protocol.
- Browser path: Chrome extension/web app -> localhost helper -> QUIC server.
- Browser fallback: plugin-free HTTP/3/WebTransport path for constrained environments.
- Multi-tenant model with per-user transfer metering.

## Quick Start

### 1) Build

```bash
cargo build
```

### 2) Run Server (release mode recommended for throughput)

```bash
cargo run --release -p metra-server -- \
  --rest-addr 127.0.0.1:8080 \
  --quic-addr 127.0.0.1:8443 \
  --data-dir /Users/ping/Projects/metra/var/data
```

### 3) Check Health (CLI)

```bash
cargo run -p metra-client -- health --output json
```

### 4) Launch TUI

```bash
cargo run -p metra-client -- tui
```

### 5) Create Transfer (scriptable CLI)

```bash
cargo run --release -p metra-client -- transfer create \
  --tenant-id tenant-a \
  --user-id user-a \
  --source-uri file:///tmp/input.bin \
  --destination-uri s3://bucket/output.bin \
  --file-name input.bin \
  --file-size-bytes 322122547200 \
  --overwrite
```

### 6) Send File Data over QUIC

```bash
cargo run --release -p metra-client -- --output json transfer send \
  --transfer-id <UUID_FROM_CREATE> \
  --file-path /tmp/input.bin \
  --io-chunk-bytes 16777216
```

### 7) Query Transfer Status

```bash
cargo run --release -p metra-client -- transfer status --transfer-id <UUID>
```

## Big File Benchmarking

Run an end-to-end benchmark (creates sparse test file, creates transfer, sends file, prints throughput):

```bash
cargo run --release -p metra-client -- --output json transfer bench \
  --size-gib 2 \
  --file-path /tmp/metra-bench-2g.bin \
  --io-chunk-bytes 16777216 \
  --lanes 4
```

## Resume Validation

1) Start a large send and interrupt it (`Ctrl+C`).
2) Re-run the same `transfer send` command with the same `transfer_id`.
3) Confirm `resumed_from_bytes` in output is non-zero.

## Measured Local Baseline (This Machine)

- Debug client + debug server (1 GiB): ~0.47 Gbps.
- Release client + debug server (1 GiB): ~0.56 Gbps.
- Release client + release server (2 GiB, 16 MiB chunks): ~0.86 Gbps.
- Release client + release server (4 GiB, 4 lanes): ~1.23 Gbps.
- Release client + release server (16 GiB, 4 lanes): ~1.21 Gbps.
- Resume retry test (8 GiB, interrupted then resumed): completed with `resumed_from_bytes = 1458886460`.

These measurements are local environment baselines and do not represent target WAN/DC performance.

## Current Limitations

- Multi-lane transfer is currently an early implementation and still needs hardening.
- Disk-backed local storage path only for data plane write target.
- Resume semantics are complete for single-lane mode; striped resume is not complete.
- No FEC, no multi-path QUIC, no congestion-controller tuning profiles yet.
- Browser helper and extension are still pending implementation.

## TODO and Plan

### Immediate (Current Iteration)

- [x] Refactor server/client monolith files into smaller modules.
- [x] Add multi-lane transfer support (`--lanes`) for parallel QUIC streams.
- [ ] Validate and tune lane scheduling for higher throughput under load.
- [ ] Add automated benchmark matrix for lane/chunk combinations.

### Near-term Performance Roadmap

- [ ] Implement bounded pipeline stages for read/encrypt-send/receive-write.
- [ ] Add per-lane and aggregate throughput metrics via OpenTelemetry.
- [ ] Add CPU and runtime tuning profile (thread affinity, buffer sizing).
- [ ] Add WAN test profiles (`tc/netem`) and record p50/p95 throughput.

### Reliability and Data Integrity

- [ ] Add robust resume for striped multi-lane transfers.
- [ ] Add per-chunk hash verification and end-of-transfer integrity checks.
- [ ] Add failure recovery tests (connection drop, lane loss, restart).

### Product Features After Core Throughput

- [ ] Implement browser helper + Chrome extension path.
- [ ] Add S3-compatible data path integration.
- [ ] Add multi-tenant fairness and transfer policies.

## Notes

- This is a first implementation slice to establish API, runtime structure, and client workflows.
- QUIC session acceptance is implemented; full chunk scheduling, resumable data streaming, FEC, and storage backends are next.
- Requirements baseline is tracked in `REQUIREMENTS.md`.
