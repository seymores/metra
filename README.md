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

Run a benchmark matrix (multiple size/lane/chunk permutations) and get one JSON summary:

```bash
cargo run --release -p metra-client -- --output json transfer matrix \
  --sizes-gib 2 \
  --lanes 2,4,8 \
  --io-chunk-bytes 16777216,67108864 \
  --file-dir /tmp \
  --cleanup-files
```

Run a no-disk benchmark (client generates payload, server uses null sink) to isolate transfer-path overhead from filesystem I/O:

```bash
cargo run --release -p metra-client -- --output json transfer bench \
  --size-gib 4 \
  --file-path /tmp/metra-bench-4g-nodisk.bin \
  --io-chunk-bytes 16777216 \
  --lanes 2 \
  --no-disk
```

Run a side-by-side compare benchmark (disk-backed + no-disk in one command):

```bash
cargo run --release -p metra-client -- --output json transfer compare \
  --size-gib 2 \
  --file-path /tmp/metra-bench-compare-2g.bin \
  --io-chunk-bytes 16777216 \
  --lanes 2 \
  --iterations 3 \
  --json-out /tmp/metra-reports/compare-2g.json \
  --cleanup-file
```

`transfer compare` and `transfer compare-series` reports now include host telemetry snapshots
(CPU, memory, load average, process memory/CPU) for each iteration.

Run a compare-series benchmark across multiple sizes:

```bash
cargo run --release -p metra-client -- --output json transfer compare-series \
  --sizes-gib 1,2 \
  --file-dir /tmp \
  --file-prefix metra-bench-compare-series \
  --io-chunk-bytes 16777216 \
  --lanes 2 \
  --iterations 2 \
  --json-out /tmp/metra-reports/compare-series-1g2g-i2.json \
  --cleanup-files
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
- Matrix (2 GiB; lanes=2/4/8; chunks=16 MiB/64 MiB): best ~1.21 Gbps at `lanes=2`, `chunk=16 MiB`.
- No-disk matrix (2 GiB; lanes=2/4; chunk=16 MiB): best ~1.92 Gbps at `lanes=2`.
- Release client + release server (16 GiB, 2 lanes, 16 MiB chunks): ~1.20 Gbps.
- Release client + release server (4 GiB, disk-backed, 2 lanes, 16 MiB chunks): ~1.18 Gbps.
- Release client + release server (4 GiB, `--no-disk`, 2 lanes, 16 MiB chunks): ~1.81 Gbps.
- Compare run (2 GiB, 2 lanes, 16 MiB): disk `~1.21 Gbps`, no-disk `~1.88 Gbps`, delta `+56.24%`.
- Compare run (2 GiB, 2 lanes, 16 MiB, 3 iterations):
  - disk p50 `~1.20 Gbps`, p95 `~1.23 Gbps`
  - no-disk p50 `~1.86 Gbps`, p95 `~1.89 Gbps`
  - delta p50 `~0.66 Gbps` (`+53.77%`), p95 `~0.67 Gbps` (`+55.55%`)
- Compare-series run (1/2 GiB, 2 lanes, 16 MiB, 2 iterations each):
  - 1 GiB p50: disk `~1.209 Gbps`, no-disk `~1.828 Gbps`, delta `~+51.18%`
  - 2 GiB p50: disk `~1.201 Gbps`, no-disk `~1.833 Gbps`, delta `~+52.60%`
- Compare-series telemetry sample (1 GiB, 2 lanes, 1 iteration): host snapshots include
  start/after-disk/after-no-disk CPU, memory, load-average, process CPU, and process memory
  with total deltas embedded in report JSON.
- Resume retry test (8 GiB, interrupted then resumed): completed with `resumed_from_bytes = 1458886460`.
- Striped resume retry test (2 GiB, 4 lanes, interrupted then resumed): completed with `resumed_from_bytes = 1879054862`.

These measurements are local environment baselines and do not represent target WAN/DC performance.

## Current Limitations

- Multi-lane transfer is currently an early implementation and still needs hardening.
- Data plane supports local disk target and null-sink benchmark target; no S3 path yet.
- Striped resume checkpointing is implemented, but still needs adversarial/fault-injection test coverage.
- No FEC, no multi-path QUIC, no congestion-controller tuning profiles yet.
- Browser helper and extension are still pending implementation.

## TODO and Plan

### Execution Plan (Prioritized)

1. Transport/runtime telemetry foundation (in progress)
   - Add per-run host/runtime metrics to benchmark artifacts.
   - Extend toward per-lane QUIC metrics and OpenTelemetry spans.
2. Hot-path performance refactor
   - Build bounded read/schedule/send and receive/write pipelines with reusable buffers.
3. QUIC tuning profiles and sweeps
   - Add `lan`, `wan`, `high-bdp` transport presets and automated parameter sweeps.
4. Striped resume hardening
   - Add atomic checkpoint persistence and adversarial restart/corruption tests.
5. WAN realism harness
   - Add `tc/netem` profiles and capture p50/p95 throughput + completion-rate reports.
6. Integrity and compliance foundations
   - Add chunk/final digest verification and structured audit log export for SIEM/S3.

### Immediate (Current Iteration)

- [x] Refactor server/client monolith files into smaller modules.
- [x] Add multi-lane transfer support (`--lanes`) for parallel QUIC streams.
- [ ] Validate and tune lane scheduling for higher throughput under load.
- [x] Add automated benchmark matrix for lane/chunk combinations (`transfer matrix`).
- [x] Add repeated compare benchmark with p50/p95 reporting (`transfer compare --iterations`).
- [x] Add multi-size compare benchmark series (`transfer compare-series`).
- [x] Add host/runtime telemetry to compare reports (start/phase/end snapshots and deltas).

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
