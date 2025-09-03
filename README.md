# Metra Transfer Protocol
Large files transfer service for the modern world.

# Metrics
- Multi-GB/TB transfer over the open Internet, faster than Aspera standard transfer
- Co-tenants
- Resistant to loss/jitter and NATS
- Consider high-BDP links -- 1-40Gps, < 200ms RTT at < 2% loss
-  90%+ provisioned bandwidth at < 0.5% loss
- Resumable, less than 1s on network blips and no slow start or stalls control
- Black-box KPIs: time-to-first-byte, time-to-verify, p50/p95 throughput, resume counts, reverse-path RTT.

# Foundation
- [QUIC - RFC9000](https://datatracker.ietf.org/doc/html/rfc9000) -- solves TLS (1.3 builtin), NAT-friendly, connection migration, multiplexing, ACK ranges and packet number spacing
  - Control = one reliable QUIC stream (session, manifests, policy, telemetry).
  - Data = many parallel QUIC streams, OR DATAGRAM frames
- Reliability
  - Chunk-level recovery: Let the app layer re-request missing chunks by ID (not byte offsets) to avoid head-of-line traps and exploit caches.
  - ACK decimation / ACK-frequency to protect the reverse path on high-rate links.
  - Selective retransmit (NACK/ACK ranges) for rare loss.
  - Adaptive systematic FEC (e.g., RaptorQ/RLNC) across stripes when loss/jitter rises. Start at 0–3% overhead, auto-tune to loss & RTT variance.
- Rate Shaping % Fairness control
  - Priority lanes
  - Business-hour caps or conservative window hours
  - Floor/Ceiling/Mas burst configuration for transfer, queue -- by user or tenant.
- BBRv2 by default (Bottleneck Bandwidth and Round-trip propagation time)

# Performance Mechanism
- Lower queuing = higher goodput under mixed traffic via model-based CC (Aspera’s classic rate shaper + loss heuristics tends to fill queues).
- Loss-masking FEC reduces retransmission stalls on 0.3–2% loss paths (satellite, Wi-Fi mesh), where pure NACK/ARQ thrashes.
- Parallel streams + chunk scheduling avoid head-of-line better than monolithic flows.
- QUIC migration + resume makes mobile/ISP NAT changes almost invisible.
- Content-defined chunking enables delta-sync & instant resume without re-seeding byte offsets; Aspera’s resume is offset-oriented.
- ACK decimation & ECN protect the reverse path (a common hidden bottleneck that slows legacy UDP senders).
- Fallback to TCP/IP

# Test Cases
1. 100GB file, 120ms RTTT, 0.5% random loss, 9Gbps+, p95 CPU -- assume 5Gpbs

# Notes

## Network tuning
- SO_RCVBUF/SO_SNDBUF to tens of MB; raise rmem_max/wmem_max.
- Enable GSO/GRO for UDP; CPU pinning for RX/TX queues; IRQ affinity; NIC coalescing tuned to pacer.
- Busy-polling (carefully) and io_uring SQPOLL for user-space stacks.
- Checksum offload and hardware crypto (AES-NI/ARMv8) detection at startup.

## Dev
- Zero-copy / low-copy pipeline: io_uring + sendmmsg/recvmmsg, fixed buffers, hugepages.
- S3/GCS “native”: multi-part, parallel PUT/GET; checksum parity with cloud ETags to avoid double hashing.
- Per-chunk BLAKE3 hash; Merkle tree root for whole-file integrity; supports concurrent verification & early finalize.

# MVP Plan

## Phase 1
- Single stream QUIC file transfer -- client and server
- BBR-like CC
- Content-defined chunking and Merkle manifest, and end-to-end hashing
- Resumable transfer

## Phase 2
- Adaptive FEC
- ACK-frequency control
- ECN, app-level multi-path stripping
- Priority queues
- Per-tenant fainess control
- HTTP/3/MASQUE tunneling (for enterprise firewalls)

## Phase 3
- Multi-path QUIC transfer
- Edge storage-promximate service -- auto-scaling
- WAN selection policy
- cross-region relay optimisation with CDN-like POP nodes
