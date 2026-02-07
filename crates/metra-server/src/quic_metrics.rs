use std::{sync::OnceLock, time::Duration};

use metra_proto::TransferStatus;
use opentelemetry::{
    KeyValue, global,
    metrics::{Counter, Histogram, UpDownCounter},
};

struct QuicMetrics {
    lane_streams_started_total: Counter<u64>,
    lane_streams_finished_total: Counter<u64>,
    lane_bytes_received_total: Counter<u64>,
    lane_duration_seconds: Histogram<f64>,
    lane_throughput_gbps: Histogram<f64>,
    transfer_finished_total: Counter<u64>,
    transfer_duration_seconds: Histogram<f64>,
    transfer_throughput_gbps: Histogram<f64>,
    active_lane_streams: UpDownCounter<i64>,
}

static QUIC_METRICS: OnceLock<QuicMetrics> = OnceLock::new();

fn metrics() -> &'static QuicMetrics {
    QUIC_METRICS.get_or_init(|| {
        let meter = global::meter("metra-server.quic");
        QuicMetrics {
            lane_streams_started_total: meter
                .u64_counter("metra.quic.lane.streams.started.total")
                .with_description("Total QUIC lane streams opened")
                .build(),
            lane_streams_finished_total: meter
                .u64_counter("metra.quic.lane.streams.finished.total")
                .with_description("Total QUIC lane streams finished by terminal status")
                .build(),
            lane_bytes_received_total: meter
                .u64_counter("metra.quic.lane.bytes.received.total")
                .with_description("Total payload bytes received by lane streams")
                .with_unit("By")
                .build(),
            lane_duration_seconds: meter
                .f64_histogram("metra.quic.lane.duration.seconds")
                .with_description("Lane stream wall-clock duration")
                .with_unit("s")
                .build(),
            lane_throughput_gbps: meter
                .f64_histogram("metra.quic.lane.throughput.gbps")
                .with_description("Lane stream throughput in gigabits per second")
                .with_unit("Gbit/s")
                .build(),
            transfer_finished_total: meter
                .u64_counter("metra.quic.transfer.finished.total")
                .with_description("Total transfers finalized by status")
                .build(),
            transfer_duration_seconds: meter
                .f64_histogram("metra.quic.transfer.duration.seconds")
                .with_description("End-to-end transfer duration from first lane stream to finalize")
                .with_unit("s")
                .build(),
            transfer_throughput_gbps: meter
                .f64_histogram("metra.quic.transfer.throughput.gbps")
                .with_description("Aggregate finalized transfer throughput in gigabits per second")
                .with_unit("Gbit/s")
                .build(),
            active_lane_streams: meter
                .i64_up_down_counter("metra.quic.lane.streams.active")
                .with_description("Currently active QUIC lane streams")
                .build(),
        }
    })
}

pub fn record_lane_stream_started(lane_index: u32, total_lanes: u32, striped: bool, no_disk: bool) {
    let metrics = metrics();
    let lane_attrs = lane_attrs(lane_index, total_lanes, striped, no_disk);
    let stream_attrs = stream_attrs(total_lanes, striped, no_disk);

    metrics.lane_streams_started_total.add(1, &lane_attrs);
    metrics.active_lane_streams.add(1, &stream_attrs);
}

pub fn record_lane_stream_finished(
    lane_index: u32,
    total_lanes: u32,
    striped: bool,
    no_disk: bool,
    status: TransferStatus,
    bytes_received: u64,
    elapsed: Duration,
) {
    let metrics = metrics();
    let status_text = status_label(status);
    let lane_attrs = lane_finish_attrs(lane_index, total_lanes, striped, no_disk, status_text);
    let stream_attrs = stream_finish_attrs(total_lanes, striped, no_disk, status_text);

    metrics.lane_streams_finished_total.add(1, &lane_attrs);
    metrics.active_lane_streams.add(-1, &stream_attrs);
    metrics
        .lane_bytes_received_total
        .add(bytes_received, &lane_attrs);

    let elapsed_secs = elapsed.as_secs_f64();
    metrics
        .lane_duration_seconds
        .record(elapsed_secs, &lane_attrs);
    if elapsed_secs > 0.0 {
        let throughput_gbps = (bytes_received as f64 * 8.0) / (elapsed_secs * 1_000_000_000.0);
        metrics
            .lane_throughput_gbps
            .record(throughput_gbps, &lane_attrs);
    }
}

pub fn record_transfer_finished(
    total_lanes: u32,
    striped: bool,
    no_disk: bool,
    status: TransferStatus,
    bytes_transferred: u64,
    elapsed: Duration,
) {
    let metrics = metrics();
    let status_text = status_label(status);
    let attrs = transfer_attrs(total_lanes, striped, no_disk, status_text);

    metrics.transfer_finished_total.add(1, &attrs);

    let elapsed_secs = elapsed.as_secs_f64();
    metrics
        .transfer_duration_seconds
        .record(elapsed_secs, &attrs);
    if elapsed_secs > 0.0 {
        let throughput_gbps = (bytes_transferred as f64 * 8.0) / (elapsed_secs * 1_000_000_000.0);
        metrics
            .transfer_throughput_gbps
            .record(throughput_gbps, &attrs);
    }
}

fn lane_attrs(lane_index: u32, total_lanes: u32, striped: bool, no_disk: bool) -> Vec<KeyValue> {
    vec![
        KeyValue::new("lane.index", lane_index as i64),
        KeyValue::new("lane.total", total_lanes as i64),
        KeyValue::new("transfer.striped", striped),
        KeyValue::new("transfer.no_disk", no_disk),
    ]
}

fn lane_finish_attrs(
    lane_index: u32,
    total_lanes: u32,
    striped: bool,
    no_disk: bool,
    status: &'static str,
) -> Vec<KeyValue> {
    let mut attrs = lane_attrs(lane_index, total_lanes, striped, no_disk);
    attrs.push(KeyValue::new("transfer.status", status));
    attrs
}

fn stream_attrs(total_lanes: u32, striped: bool, no_disk: bool) -> Vec<KeyValue> {
    vec![
        KeyValue::new("lane.total", total_lanes as i64),
        KeyValue::new("transfer.striped", striped),
        KeyValue::new("transfer.no_disk", no_disk),
    ]
}

fn stream_finish_attrs(
    total_lanes: u32,
    striped: bool,
    no_disk: bool,
    status: &'static str,
) -> Vec<KeyValue> {
    let mut attrs = stream_attrs(total_lanes, striped, no_disk);
    attrs.push(KeyValue::new("transfer.status", status));
    attrs
}

fn transfer_attrs(
    total_lanes: u32,
    striped: bool,
    no_disk: bool,
    status: &'static str,
) -> Vec<KeyValue> {
    vec![
        KeyValue::new("lane.total", total_lanes as i64),
        KeyValue::new("transfer.striped", striped),
        KeyValue::new("transfer.no_disk", no_disk),
        KeyValue::new("transfer.status", status),
    ]
}

fn status_label(status: TransferStatus) -> &'static str {
    match status {
        TransferStatus::Queued => "queued",
        TransferStatus::Running => "running",
        TransferStatus::Completed => "completed",
        TransferStatus::Failed => "failed",
    }
}
