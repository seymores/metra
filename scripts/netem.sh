#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  netem.sh apply <profile> [dev]
  netem.sh clear [dev]
  netem.sh show [dev]

Profiles:
  latency  : delay-focused WAN simulation (80ms +/- 10ms)
  loss     : packet-loss-focused WAN simulation (40ms +/- 5ms, 0.5% loss)
  jitter   : jitter-focused WAN simulation (60ms +/- 30ms, 0.1% loss)
EOF
}

require_tc() {
  if ! command -v tc >/dev/null 2>&1; then
    echo "tc command not found; install iproute2" >&2
    exit 1
  fi
}

apply_profile() {
  local profile="$1"
  local dev="$2"
  case "$profile" in
    latency)
      tc qdisc replace dev "$dev" root netem delay 80ms 10ms distribution normal
      ;;
    loss)
      tc qdisc replace dev "$dev" root netem delay 40ms 5ms distribution normal loss 0.5%
      ;;
    jitter)
      tc qdisc replace dev "$dev" root netem delay 60ms 30ms distribution normal loss 0.1%
      ;;
    *)
      echo "unknown netem profile: $profile" >&2
      usage
      exit 2
      ;;
  esac
}

main() {
  require_tc
  local action="${1:-}"
  local value="${2:-}"
  local dev="${3:-lo}"

  case "$action" in
    apply)
      if [[ -z "$value" ]]; then
        usage
        exit 2
      fi
      apply_profile "$value" "$dev"
      tc qdisc show dev "$dev"
      ;;
    clear)
      tc qdisc del dev "${value:-$dev}" root 2>/dev/null || true
      tc qdisc show dev "${value:-$dev}"
      ;;
    show)
      tc qdisc show dev "${value:-$dev}"
      ;;
    *)
      usage
      exit 2
      ;;
  esac
}

main "$@"
