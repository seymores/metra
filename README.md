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
  --data-dir /Users/ping/Projects/metra/var/data \
  --quic-profile lan
```

`--quic-profile` options:

- `lan`: low-latency datacenter/LAN baseline.
- `wan`: lower receive/send windows with longer idle timeout.
- `high-bdp`: larger windows for long-fat networks.

### 3) Check Health (CLI)

```bash
cargo run -p metra-client -- health --output json
```

### 4) Launch TUI

```bash
cargo run -p metra-client -- tui
```

TUI keys:
- `r`: refresh health
- `b`: run benchmark with current TUI benchmark config
- `q`: quit

TUI benchmark runtime auto-selection options:

```bash
cargo run -p metra-client -- tui \
  --bench-size-gib 1 \
  --bench-lanes 2 \
  --bench-io-chunk-bytes 8388608 \
  --bench-no-disk \
  --runtime-policy /tmp/metra-reports/runtime-policy.json
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
  --io-chunk-bytes 16777216 \
  --auto-runtime-report /tmp/metra-reports/tune-runtime-1g-l2.json
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

Run automated matrix sweep across requested QUIC profiles (optionally against per-profile servers):

```bash
cargo run --release -p metra-client -- --output json transfer matrix-profiles \
  --profiles lan,wan,high-bdp \
  --servers http://127.0.0.1:8080,http://127.0.0.1:8081,http://127.0.0.1:8082 \
  --sizes-gib 1,2 \
  --lanes 1,2 \
  --io-chunk-bytes 16777216 \
  --file-dir /tmp \
  --cleanup-files
```

If `--servers` is omitted, all profiles run against the global `--server`. Output includes
`detected_profile`, `profile_match`, and `profile_note` from `/health`.

Run runtime profile sweep (`balanced`, `throughput`, `low-cpu`) and get recommended profile by p50 throughput:

```bash
cargo run --release -p metra-client -- --output json transfer tune-runtime \
  --size-gib 1 \
  --lanes 2 \
  --iterations 2 \
  --io-chunk-bytes 8388608 \
  --no-disk \
  --json-out /tmp/metra-reports/tune-runtime-1g-l2.json \
  --runtime-policy-out /tmp/metra-reports/runtime-policy.json
```

`transfer send`, `transfer bench`, and `transfer matrix` also accept:
- `--runtime-profile balanced|throughput|low-cpu`
- `--file-read-pipeline-depth <N>`
- `--auto-runtime-report <PATH_TO_TUNE_RUNTIME_REPORT_JSON>`
- `--runtime-policy <PATH_TO_RUNTIME_POLICY_JSON>`

Runtime selection precedence:
- explicit `--runtime-profile`
- `--auto-runtime-report`
- `--runtime-policy`

Use a tune-runtime report to auto-select runtime profile for matching benchmark workloads:

```bash
cargo run --release -p metra-client -- --output json transfer bench \
  --size-gib 1 \
  --file-path /tmp/metra-auto-runtime-report.bin \
  --io-chunk-bytes 8388608 \
  --lanes 2 \
  --no-disk \
  --auto-runtime-report /tmp/metra-reports/tune-runtime-1g-l2.json
```

Use persisted runtime policy to auto-select runtime profile with nearest-workload fallback:

```bash
cargo run --release -p metra-client -- --output json transfer bench \
  --size-gib 1 \
  --file-path /tmp/metra-auto-runtime-policy.bin \
  --io-chunk-bytes 8388608 \
  --lanes 4 \
  --no-disk \
  --runtime-policy /tmp/metra-reports/runtime-policy.json
```

Auto runtime-profile selection from reports/policy is available for `transfer send`,
`transfer bench`, `transfer matrix`, and TUI benchmark runs.

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

Run lane tuning under concurrent load (sweeps lane counts, reports p50/p95, recommends lane count):

```bash
cargo run --release -p metra-client -- --output json transfer tune-lanes \
  --size-gib 1 \
  --lanes 1,2,4 \
  --concurrency 2 \
  --iterations 2 \
  --io-chunk-bytes 16777216 \
  --no-disk \
  --json-out /tmp/metra-reports/tune-lanes-1g-c2-i2.json \
  --lane-policy-out /tmp/metra-reports/lane-policy.json \
  --cleanup-file
```

Use a previously exported tune-lanes report to auto-select lane count for new benchmarks:

```bash
cargo run --release -p metra-client -- --output json transfer bench \
  --size-gib 1 \
  --file-path /tmp/metra-auto-lane-1g.bin \
  --io-chunk-bytes 16777216 \
  --lanes 1 \
  --no-disk \
  --auto-lanes-report /tmp/metra-reports/tune-lanes-1g-c2-i2.json
```

Use persisted lane policy (from one or more tune runs) to auto-select lanes with workload fallback:

```bash
cargo run --release -p metra-client -- --output json transfer bench \
  --size-gib 2 \
  --file-path /tmp/metra-auto-policy-2g.bin \
  --io-chunk-bytes 16777216 \
  --lanes 1 \
  --no-disk \
  --lane-policy /tmp/metra-reports/lane-policy.json
```

`--auto-lanes-report` takes precedence over `--lane-policy` when both are provided.

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
- Compare run (1 GiB, 2 lanes, 16 MiB, 2 iterations, post striped-null finalize fix):
  - disk p50 `~1.303 Gbps`, p95 `~1.304 Gbps`
  - no-disk p50 `~2.168 Gbps`, p95 `~2.171 Gbps`
  - delta p50 `~0.865 Gbps` (`+66.40%`)
- Runtime tuning sample (1 GiB, 2 lanes, no-disk, 1 iteration/profile):
  - balanced `~2.043 Gbps`, throughput `~2.034 Gbps`, low-cpu `~2.045 Gbps`
  - recommended profile: `low-cpu` (on this machine)
- Tune-lanes run (1 GiB, concurrency=2, no-disk, lanes=1/2, 1 iteration):
  - recommended lanes: `2`
  - lane 1 aggregate: `~1.90 Gbps`
  - lane 2 aggregate: `~2.20 Gbps`
- Auto-lane bench sample (1 GiB no-disk, configured `--lanes 1` + tune report):
  - effective lanes: `2` (auto-selected from report)
  - achieved throughput: `~1.72 Gbps`
- Lane-policy bench sample (2 GiB no-disk, configured `--lanes 4` + persisted policy):
  - effective lanes: `1` (exact profile match from policy)
  - achieved throughput: `~1.91 Gbps`
- Lane-policy fallback sample (3 GiB no-disk, configured `--lanes 4` + persisted policy):
  - effective lanes: `1` (fallback to nearest profile `size=2 GiB`, `concurrency=1`)
  - achieved throughput: `~1.84 Gbps`
- Resume retry test (8 GiB, interrupted then resumed): completed with `resumed_from_bytes = 1458886460`.
- Striped resume retry test (2 GiB, 4 lanes, interrupted then resumed): completed with `resumed_from_bytes = 1879054862`.

These measurements are local environment baselines and do not represent target WAN/DC performance.

## OpenTelemetry Metrics

Server-side QUIC data path now records OpenTelemetry metrics for:

- per-lane stream lifecycle and bytes (`metra.quic.lane.streams.*`, `metra.quic.lane.bytes.received.total`)
- per-lane duration and throughput (`metra.quic.lane.duration.seconds`, `metra.quic.lane.throughput.gbps`)
- aggregate transfer finalize duration and throughput (`metra.quic.transfer.duration.seconds`, `metra.quic.transfer.throughput.gbps`)
- active lane stream concurrency (`metra.quic.lane.streams.active`)

## CI Benchmark Gate

- Workflow: `.github/workflows/benchmark-gate.yml`
- Baseline config: `ci/benchmark-baseline.json`
- Gate script: `scripts/ci/benchmark_gate.py`

The gate runs `transfer tune-runtime` against localhost (`--no-disk`) and fails CI if any
runtime profile p50 throughput regresses below baseline threshold.

## Current Limitations

- Multi-lane transfer is currently an early implementation and still needs hardening.
- Data plane supports local disk target and null-sink benchmark target; no S3 path yet.
- Striped resume checkpointing is implemented, but still needs adversarial/fault-injection test coverage.
- No FEC, no multi-path QUIC, and no congestion-controller tuning sweep harness yet.
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
- [x] Validate and tune lane scheduling for higher throughput under load (`transfer tune-lanes`).
- [x] Add automated benchmark matrix for lane/chunk combinations (`transfer matrix`).
- [x] Add repeated compare benchmark with p50/p95 reporting (`transfer compare --iterations`).
- [x] Add multi-size compare benchmark series (`transfer compare-series`).
- [x] Add host/runtime telemetry to compare reports (start/phase/end snapshots and deltas).
- [x] Add adaptive lane policy that auto-selects from recent tune-lanes reports.
- [x] Add lane-policy persistence by workload profile (size/concurrency) and automatic fallback.
- [x] Add server QUIC transport profiles (`--quic-profile lan|wan|high-bdp`).
- [x] Add runtime profile presets and profile-sweep benchmark (`transfer tune-runtime`).
- [x] Add runtime-policy persistence by workload profile (`--runtime-policy-out`) and automatic fallback (`--runtime-policy`).
- [x] Add auto runtime-profile selection from tune-runtime reports (`--auto-runtime-report`).
- [x] Fix striped null-sink transfer completion/status accounting (`--no-disk` multi-lane).
- [x] Add CI runtime-profile regression gate (`benchmark-gate` workflow).

### Near-term Performance Roadmap

- [ ] Implement bounded pipeline stages for read/encrypt-send/receive-write.
  - Client file-source read/send path now uses bounded pipelining with backpressure and buffer reuse.
  - Server receive/write path now uses bounded read->write pipelining with backpressure.
- [x] Add per-lane and aggregate throughput metrics via OpenTelemetry.
- [x] Add CPU/runtime tuning profile presets (chunk + pipeline depth) and sweep command.
- [x] Add adaptive runtime auto-selection from persisted tune-runtime profiles.
- [ ] Add WAN test profiles (`tc/netem`) and record p50/p95 throughput.
- [x] Add automated benchmark profile sweep (`transfer matrix-profiles`) with profile detection.

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
