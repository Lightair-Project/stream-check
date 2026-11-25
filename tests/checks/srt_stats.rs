//! SRT link statistics: loss and drops judged over a window, with a warning above 2% loss and on
//! any drop, and counters that add up across a session's reconnects. The receiver's counters
//! are given as the SRT socket reports them.

use srt_protocol::statistics::SocketStatistics;
use stream_check::analyze::Level;
use stream_check::srt_stats::{SrtLink, SrtStats};

/// What the socket reports after this many packets: first arrivals, filled gaps, given up.
fn socket(unique: u64, recovered: u64, dropped: u64) -> SocketStatistics {
    let mut s = SocketStatistics::default();
    s.rx_unique_data = unique;
    s.rx_retransmit_data = recovered;
    s.rx_dropped_data = dropped;
    s.rx_data = unique + recovered;
    s.rx_bytes = (unique + recovered) * 1316;
    s
}

fn connected() -> SrtStats {
    let mut s = SrtStats::default();
    s.begin(SrtLink { latency_ms: 120, ..Default::default() });
    s
}

#[test]
fn loss_above_two_percent_is_warned_once_and_cleared_when_it_falls_back() {
    let mut s = connected();
    s.update(0.0, &socket(0, 0, 0), 0);
    // second 1: 50 of 1050 packets missing on first arrival, all recovered
    let notes = s.update(1.0, &socket(1000, 50, 0), 0);
    assert_eq!(notes.iter().filter(|n| n.0 == Level::Warn).count(), 1, "{notes:?}");
    assert!(s.loss_warning);
    let mut later = vec![];
    for t in 2..=8 {
        later.extend(s.update(t as f64, &socket(1000 * t, 50, 0), 0));
    }
    assert!(!later.iter().any(|n| n.0 == Level::Warn), "said once: {later:?}");
    assert!(later.iter().any(|n| n.0 == Level::Info), "cleared: {later:?}");
    assert!(!s.loss_warning);
    assert_eq!((s.total.missing, s.total.recovered, s.total.dropped), (50, 50, 0));
}

#[test]
fn loss_under_two_percent_is_not_a_warning() {
    let mut s = connected();
    s.update(0.0, &socket(0, 0, 0), 0);
    let mut notes = vec![];
    for t in 1..=10u64 {
        // 1% missing every second
        notes.extend(s.update(t as f64, &socket(990 * t, 10 * t, 0), 0));
    }
    assert!(notes.is_empty(), "{notes:?}");
    assert!((s.loss_pct - 1.0).abs() < 0.01, "{}", s.loss_pct);
}

#[test]
fn a_dropped_packet_is_a_warning_until_fifteen_seconds_pass_without_one() {
    let mut s = connected();
    s.update(0.0, &socket(0, 0, 0), 0);
    let notes = s.update(1.0, &socket(1000, 0, 1), 0);
    assert!(notes.iter().any(|n| n.0 == Level::Warn && n.1.contains("dropped")), "{notes:?}");
    assert!(s.drop_warning);
    for t in 2..=15u64 {
        s.update(t as f64, &socket(1000 * t, 0, 1), 0);
        assert!(s.drop_warning, "still at {t} s");
    }
    let notes = s.update(16.0, &socket(16_000, 0, 1), 0);
    assert!(!s.drop_warning, "{notes:?}");
}

#[test]
fn counters_add_up_across_the_sessions_connections() {
    let mut s = connected();
    s.update(0.0, &socket(0, 0, 0), 0);
    s.update(1.0, &socket(500, 5, 1), 3);
    s.finish();
    // the sender reconnects: its new socket counts from zero
    s.begin(SrtLink::default());
    s.update(2.0, &socket(0, 0, 0), 0);
    s.update(3.0, &socket(200, 2, 0), 4);
    assert_eq!(s.connections, 2);
    assert_eq!((s.total.unique, s.total.recovered, s.total.dropped, s.total.fault_dropped), (700, 7, 1, 7));
    assert_eq!((s.current.unique, s.current.recovered), (200, 2));
}
