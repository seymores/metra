# Metra Requirements (v1)

Date: 2026-02-06  
Product codename: Metra

## 1) Product Goals

- Build an open-source large-file transfer system that matches or exceeds IBM Aspera transfer speed for equivalent workloads.
- License the project under Apache License 2.0 for broad adoption.
- Support commercial SaaS operation while remaining open source.
- Start as a clean-slate system (no Aspera protocol compatibility requirement).

## 2) Delivery Milestones

- First iteration (vertical slice): 2026-02-06 (tonight).
- Complete solution target: 2026-02-09.

## 3) Architecture Decisions

- Language: Rust for core/server/client implementation.
- Protocol split (locked for v1):
  - Control plane: REST API.
  - Data plane: custom QUIC transfer protocol.
  - Browser fallback: HTTP/3/WebTransport path with lower expected performance.
- Deployment model:
  - Multi-region capable.
  - Private datacenter support.
  - Containerized runtime required.
  - Keep operations simple enough for one SRE/developer.

## 4) Performance and Scale Requirements

- Primary workload file sizes: 300 GB to 800 GB.
- Per-transfer performance target:
  - 200 Gbps average where infrastructure permits, or
  - >=80% of available path bandwidth saturation.
- Concurrency baseline: 20 concurrent users (CCU) initially.
- Scale goal: horizontal scale to N concurrent 200 Gbps transfers.
- Reliability target: 99.9% transfer success rate.
- Service availability SLA target: 99.9%.
- Network conditions: must handle common internet inter-region conditions, including variable latency and packet loss.

## 5) Transfer Semantics and Data Integrity

- File transfer only in v1 (directory sync is out of scope for v1).
- Resume/checkpoint required at 1 MiB chunk granularity.
- Destination handling must support:
  - overwrite policies,
  - versioning policies,
  - immutable destination mode.
- Integrity requirements:
  - per-chunk and/or per-file hashing,
  - signed transfer manifests.
- Performance control:
  - app-level pacing required,
  - forward error correction (FEC) support required.

## 6) Client Requirements (v1)

- First client priority: TUI client-server workflow.
- Client must include scriptable CLI commands in the same first-iteration binary.
- OS support for v1 clients:
  - Linux,
  - macOS.
- Background transfer queueing and offline retry required.
- Client implementation may remain language-agnostic at interface level (public protocol/API contract and SDK-ready design).

## 7) Browser Transfer Requirements (Aspera Connect-like)

- Browser-initiated high-speed transfers must be supported via local helper model.
- Required flow:
  - Chrome extension/web app communicates with local daemon over loopback (localhost).
  - Local daemon communicates with transfer server over QUIC.
- Browser/helper support in v1:
  - Chrome browser.
  - Local helper support for Linux and macOS.
- Enterprise controls:
  - silent install/update support for helper.
- Fallback path:
  - plugin-free browser transfer fallback required (HTTP/3/WebTransport).
- Browser upload constraints in v1:
  - enforce max files per job limit (numeric value to be finalized).

## 8) Security, Compliance, and Multi-Tenancy

- Security/compliance targets:
  - SOC 2 alignment.
  - HIPAA-aligned controls first; formal certification later.
- Encryption:
  - end-to-end payload encryption required,
  - customer-managed keys required.
- Identity and access:
  - SSO (OIDC/SAML) support required.
  - API key auth support required.
- Multi-tenancy:
  - multiple user accounts/tenants required (Aspera-like account model).
- Audit and logging:
  - access logs and file transfer logs required.
  - support exporting logs to SIEM destinations, including S3-compatible targets.
  - retention/tamper-evidence should follow current best practices.

## 9) Storage and Transfer Topologies

- Storage backends required in v1:
  - local filesystem,
  - S3-compatible object storage.
- Transfer mode:
  - hybrid architecture (server-relayed and direct/object-store-aware where appropriate).

## 10) Observability and Metering

- Observability standard: OpenTelemetry required.
- Metering required in v1:
  - bytes transferred,
  - user account dimension.

## 11) Cost and Operability Constraints

- Design objective: lowest practical operating cost.
- System should be operable by a single SRE/developer.
- Prefer operational simplicity in early deployment model while keeping a path to multi-region scale.

## 12) Open Decisions (Still Needed)

- Numeric value for "max files per job" in v1 (currently defined as required limit, value TBD).
