//! SRT link statistics of a session: what the SRT receiver saw on each connection (packets,
//! gaps filled by retransmission, packets dropped too late, receive rate), a
//! time series for graphs, and warnings when loss or drops pass a threshold.
//!
//! The SRT receiver (srt-protocol 0.4) doesn't count lost packets or measure the round trip after
//! the handshake (and its receive buffer's fill isn't reliable under loss), so the lab derives them: a packet missing on first arrival is one that later
//! filled its gap (recovered) or was given up on (dropped); the round trip is the handshake's.

use std::collections::VecDeque;
use std::sync::{Arc, Weak};

use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{Value, json};
use srt_protocol::connection::ConnectionSettings;
use srt_protocol::statistics::SocketStatistics;

use crate::analyze::Level;
use crate::hub::Session;

/// Loss (packets missing on first arrival) above this share of the window's packets is a warning.
pub const LOSS_WARN_PCT: f64 = 2.0;
/// Loss falls back under this share before the warning is cleared.
pub const LOSS_CLEAR_PCT: f64 = 1.0;
/// The window loss and drops are judged over, in seconds.
pub const WINDOW_S: f64 = 5.0;
/// Seconds without drops before the drop warning is cleared.
pub const DROP_CLEAR_S: f64 = 15.0;
/// Seconds without a data packet, the connection still open, before the sender is said to have
/// gone quiet.
pub const SILENT_S: f64 = 3.0;
/// Samples kept for the time series (one a second).
const MAX_SAMPLES: usize = 7200;
/// Samples a report keeps at most (the series is thinned to fit).
const REPORT_SAMPLES: usize = 600;

/// How the connection was set up: negotiated in the SRT handshake.
#[derive(Debug, Default, Clone, Serialize)]
pub struct SrtLink {
    pub conn: u64,
    pub remote: String,
    pub stream_id: String,
    /// The receive latency both sides agreed on (the larger of the two sides' settings), in ms.
    pub latency_ms: u64,
    /// The lab's own receive latency setting, in ms.
    pub lab_latency_ms: u64,
    /// The latency agreed for the other direction (the lab to the sender), in ms.
    pub peer_latency_ms: u64,
    /// The round trip measured during the handshake, in ms.
    pub handshake_rtt_ms: f64,
    /// The flow window, in packets: how many packets may be in flight unacknowledged.
    pub flow_window_pkts: u64,
    pub max_packet_bytes: u64,
    /// The receive buffer, in packets.
    pub recv_buffer_pkts: u64,
    /// Whether packets too late to play are dropped rather than waited for.
    pub too_late_drop: bool,
    /// AES key length in bits, when the stream is encrypted.
    pub encryption_bits: Option<u32>,
    /// Seconds into the session when the connection opened.
    pub started_s: f64,
}

/// Packet counters of the receiving side.
#[derive(Debug, Default, Clone, Copy, PartialEq, Serialize)]
pub struct SrtCounters {
    /// Data packets received, retransmissions included.
    pub packets: u64,
    /// Data packets received in order the first time.
    pub unique: u64,
    /// Packets that arrived late (retransmitted or reordered) and filled their gap in time.
    pub recovered: u64,
    /// Packets given up on: too late to play.
    pub dropped: u64,
    /// Packets that arrived after their place had been played or given up on: retransmissions that
    /// weren't needed (the first copy had arrived; the sender's retransmission timer fired before
    /// the acknowledgement reached it, as under load) or came too late. Not loss.
    pub late: u64,
    /// Packets missing on first arrival: recovered plus dropped.
    pub missing: u64,
    /// Transport stream datagrams the lab threw away after SRT (the `loss` fault).
    pub fault_dropped: u64,
    pub decrypt_errors: u64,
    /// Loss reports (NAKs) the lab sent asking for retransmission.
    pub naks_sent: u64,
    pub acks_sent: u64,
    /// Bytes of data packets received, SRT headers included.
    pub bytes: u64,
    /// Times the receiver adjusted its clock to the sender's.
    pub clock_adjustments: u64,
}

impl SrtCounters {
    fn from_socket(s: &SocketStatistics, fault_dropped: u64) -> Self {
        // srt-protocol counts a packet arriving after its place was released (a needless
        // retransmission of a packet already played, say) as dropped, like one given up on.
        // Arrivals that were neither first-time nor gap-filling are those (or duplicates still in
        // the buffer, which aren't counted as dropped): taken out of the drops.
        let extra = s.rx_data.saturating_sub(s.rx_unique_data + s.rx_retransmit_data + s.rx_decrypt_errors);
        let late = extra.min(s.rx_dropped_data);
        let dropped = s.rx_dropped_data - late;
        SrtCounters {
            packets: s.rx_data,
            unique: s.rx_unique_data,
            recovered: s.rx_retransmit_data,
            dropped,
            late,
            missing: s.rx_retransmit_data + dropped,
            fault_dropped,
            decrypt_errors: s.rx_decrypt_errors,
            naks_sent: s.tx_nak,
            acks_sent: s.tx_ack,
            bytes: s.rx_bytes,
            clock_adjustments: s.rx_clock_adjustments,
        }
    }

    fn add(self, o: SrtCounters) -> Self {
        SrtCounters {
            packets: self.packets + o.packets,
            unique: self.unique + o.unique,
            recovered: self.recovered + o.recovered,
            dropped: self.dropped + o.dropped,
            late: self.late + o.late,
            missing: self.missing + o.missing,
            fault_dropped: self.fault_dropped + o.fault_dropped,
            decrypt_errors: self.decrypt_errors + o.decrypt_errors,
            naks_sent: self.naks_sent + o.naks_sent,
            acks_sent: self.acks_sent + o.acks_sent,
            bytes: self.bytes + o.bytes,
            clock_adjustments: self.clock_adjustments + o.clock_adjustments,
        }
    }

    /// `self - o`, never below zero (the counters only grow within a connection).
    fn since(self, o: SrtCounters) -> Self {
        SrtCounters {
            packets: self.packets.saturating_sub(o.packets),
            unique: self.unique.saturating_sub(o.unique),
            recovered: self.recovered.saturating_sub(o.recovered),
            dropped: self.dropped.saturating_sub(o.dropped),
            late: self.late.saturating_sub(o.late),
            missing: self.missing.saturating_sub(o.missing),
            fault_dropped: self.fault_dropped.saturating_sub(o.fault_dropped),
            decrypt_errors: self.decrypt_errors.saturating_sub(o.decrypt_errors),
            naks_sent: self.naks_sent.saturating_sub(o.naks_sent),
            acks_sent: self.acks_sent.saturating_sub(o.acks_sent),
            bytes: self.bytes.saturating_sub(o.bytes),
            clock_adjustments: self.clock_adjustments.saturating_sub(o.clock_adjustments),
        }
    }

    /// Missing packets as a share of all the packets that should have arrived, in percent.
    fn loss_pct(&self) -> f64 {
        pct(self.missing, self.unique + self.missing)
    }

    /// Dropped packets as a share of all the packets that should have arrived, in percent.
    fn drop_pct(&self) -> f64 {
        pct(self.dropped, self.unique + self.missing)
    }
}

fn pct(n: u64, of: u64) -> f64 {
    if of == 0 { 0.0 } else { n as f64 * 100.0 / of as f64 }
}

/// One second of the link.
#[derive(Debug, Default, Clone, Serialize)]
pub struct SrtSample {
    pub t: f64,
    /// Receive rate, SRT headers included.
    pub kbps: f64,
    pub pps: f64,
    pub recovered: u64,
    pub dropped: u64,
    /// Missing on first arrival, in percent of the second's packets.
    pub loss_pct: f64,
}

/// A session's SRT statistics, over all its SRT connections.
#[derive(Debug, Default, Clone, Serialize)]
pub struct SrtStats {
    /// The latest connection's setup.
    pub link: Option<SrtLink>,
    pub connections: u32,
    pub connected: bool,
    /// All connections together.
    pub total: SrtCounters,
    /// The latest connection.
    pub current: SrtCounters,
    /// Missing on first arrival, over the session, in percent.
    pub loss_pct: f64,
    /// Dropped, over the session, in percent.
    pub drop_pct: f64,
    /// Missing on first arrival over the last `WINDOW_S` seconds, in percent.
    pub window_loss_pct: f64,
    /// Dropped over the last `WINDOW_S` seconds.
    pub window_dropped: u64,
    pub kbps: f64,
    pub peak_kbps: f64,
    pub pps: f64,
    /// Loss is above `LOSS_WARN_PCT` right now.
    pub loss_warning: bool,
    /// Packets were dropped in the last `DROP_CLEAR_S` seconds.
    pub drop_warning: bool,
    pub thresholds: Thresholds,
    #[serde(skip)]
    series: Vec<SrtSample>,
    #[serde(skip)]
    earlier: SrtCounters,
    #[serde(skip)]
    last: Option<(f64, SrtCounters)>,
    #[serde(skip)]
    window: VecDeque<(f64, SrtCounters)>,
    #[serde(skip)]
    decrypt_warned: u64,
    #[serde(skip)]
    last_drop_t: f64,
    /// The connection is open but no data packet arrived in the last `SILENT_S` seconds.
    pub silent_warning: bool,
    /// Times a sender went quiet that way, over the session.
    pub silences: u32,
    /// When the latest connection's last data packet arrived (or it opened).
    #[serde(skip)]
    last_data_t: f64,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Thresholds {
    pub loss_warn_pct: f64,
    pub loss_clear_pct: f64,
    pub window_s: f64,
    pub drop_clear_s: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Thresholds { loss_warn_pct: LOSS_WARN_PCT, loss_clear_pct: LOSS_CLEAR_PCT, window_s: WINDOW_S, drop_clear_s: DROP_CLEAR_S }
    }
}

impl SrtStats {
    /// A connection opened: its counters start from zero.
    pub fn begin(&mut self, link: SrtLink) {
        self.finish();
        self.connections += 1;
        self.connected = true;
        self.current = SrtCounters::default();
        self.last = None;
        self.window.clear();
        self.last_data_t = link.started_s;
        self.silent_warning = false;
        self.link = Some(link);
    }

    /// The connection closed: its counters join the earlier connections'.
    pub fn finish(&mut self) {
        if self.connected {
            self.earlier = self.earlier.add(self.current);
            self.current = SrtCounters::default();
            self.connected = false;
            self.kbps = 0.0;
            self.pps = 0.0;
        }
    }

    /// New statistics from the SRT socket at `t` seconds into the session; returns the event
    /// log lines it gives rise to.
    pub fn update(&mut self, t: f64, s: &SocketStatistics, fault_dropped: u64) -> Vec<(Level, String)> {
        let mut out = vec![];
        if !self.connected {
            return out;
        }
        let now = SrtCounters::from_socket(s, fault_dropped);
        self.current = now;
        self.total = self.earlier.add(now);
        self.loss_pct = self.total.loss_pct();
        self.drop_pct = self.total.drop_pct();
        let arrived = now.packets > self.last.map_or(0, |(_, c)| c.packets);
        if arrived {
            self.last_data_t = t;
        }
        let Some((t0, before)) = self.last.replace((t, now)) else { return out };
        let dt = t - t0;
        if dt <= 0.0 {
            return out;
        }
        let d = now.since(before);
        self.kbps = d.bytes as f64 * 8.0 / dt / 1000.0;
        self.pps = d.packets as f64 / dt;
        self.peak_kbps = self.peak_kbps.max(self.kbps);
        self.window.push_back((t, d));
        while self.window.front().is_some_and(|(at, _)| t - at >= WINDOW_S) {
            self.window.pop_front();
        }
        let w = self.window.iter().fold(SrtCounters::default(), |a, (_, d)| a.add(*d));
        self.window_loss_pct = w.loss_pct();
        self.window_dropped = w.dropped;
        self.series.push(SrtSample {
            t,
            kbps: self.kbps,
            pps: self.pps,
            recovered: d.recovered,
            dropped: d.dropped,
            loss_pct: d.loss_pct(),
        });
        if self.series.len() > MAX_SAMPLES {
            self.series.remove(0);
        }

        let latency = self.link.as_ref().map_or(0, |l| l.latency_ms);
        if !self.loss_warning && self.window_loss_pct > LOSS_WARN_PCT {
            self.loss_warning = true;
            out.push((
                Level::Warn,
                format!(
                    "SRT: {:.1}% of packets missing on first arrival over the last {WINDOW_S:.0} s ({} recovered by retransmission, {} dropped)",
                    self.window_loss_pct, w.recovered, w.dropped
                ),
            ));
        } else if self.loss_warning && self.window_loss_pct < LOSS_CLEAR_PCT {
            self.loss_warning = false;
            out.push((Level::Info, format!("SRT: packet loss back under {LOSS_CLEAR_PCT}%")));
        }
        if d.dropped > 0 {
            self.last_drop_t = t;
        }
        if !self.drop_warning && w.dropped > 0 {
            self.drop_warning = true;
            out.push((
                Level::Warn,
                format!(
                    "SRT: {} packet(s) dropped, too late to be recovered within the {latency} ms receive latency: pictures or sound are damaged",
                    w.dropped
                ),
            ));
        } else if self.drop_warning && t - self.last_drop_t >= DROP_CLEAR_S {
            self.drop_warning = false;
            out.push((Level::Info, format!("SRT: no packets dropped in the last {DROP_CLEAR_S:.0} s")));
        }
        if arrived {
            if self.silent_warning {
                self.silent_warning = false;
                out.push((Level::Info, "SRT: data packets are arriving again".to_string()));
            }
        } else if !self.silent_warning && t - self.last_data_t >= SILENT_S {
            self.silent_warning = true;
            self.silences += 1;
            let what = if now.packets == 0 {
                "no data packet has arrived since it opened".to_string()
            } else {
                format!("no data packet has arrived for {:.0} s, after {} packet(s)", t - self.last_data_t, now.packets)
            };
            out.push((
                Level::Warn,
                format!(
                    "SRT: the connection is open (the sender answers) but {what}: the sender stopped sending.                      An srt-tokio sender does this for good when more datagrams wait than its send buffer holds                      (32 by default): it drops the unsent ones and never sends again; give it a larger sender buffer_size"
                ),
            ));
        }
        if self.total.decrypt_errors > self.decrypt_warned {
            out.push((Level::Warn, format!("SRT: {} packet(s) could not be decrypted", self.total.decrypt_errors - self.decrypt_warned)));
            self.decrypt_warned = self.total.decrypt_errors;
        }
        out
    }

    /// The statistics with the last `samples` seconds of the series (`0`: the whole series,
    /// thinned to at most 600 samples).
    pub fn to_json(&self, samples: usize) -> Value {
        let series: Vec<&SrtSample> = if samples == 0 {
            self.series.iter().step_by(self.series.len() / REPORT_SAMPLES + 1).collect()
        } else {
            self.series.iter().skip(self.series.len().saturating_sub(samples)).collect()
        };
        let mut v = json!(self);
        v["series"] = json!(series);
        v
    }
}

/// A session and its SRT statistics.
type Entry = (Weak<Session>, Arc<Mutex<SrtStats>>);

/// Every session's SRT statistics, held for as long as the session exists.
static REGISTRY: Mutex<Vec<Entry>> = Mutex::new(Vec::new());

/// The session's SRT statistics, made when its first SRT connection opens.
pub fn for_session(session: &Arc<Session>) -> Arc<Mutex<SrtStats>> {
    let mut r = REGISTRY.lock();
    r.retain(|(w, _)| w.strong_count() > 0);
    if let Some((_, s)) = r.iter().find(|(w, _)| std::ptr::eq(w.as_ptr(), Arc::as_ptr(session))) {
        return s.clone();
    }
    let s = Arc::new(Mutex::new(SrtStats::default()));
    r.push((Arc::downgrade(session), s.clone()));
    s
}

/// The session's SRT statistics as JSON (see `SrtStats::to_json`), or null when it never had an
/// SRT connection.
pub fn stats_json(session: &Session, samples: usize) -> Value {
    let s = REGISTRY.lock().iter().find(|(w, _)| std::ptr::eq(w.as_ptr(), session)).map(|(_, s)| s.clone());
    s.map_or(Value::Null, |s| s.lock().to_json(samples))
}

/// The report's problems from the session's SRT statistics.
pub fn problems(session: &Session) -> Vec<String> {
    let s = REGISTRY.lock().iter().find(|(w, _)| std::ptr::eq(w.as_ptr(), session)).map(|(_, s)| s.clone());
    let Some(s) = s else { return vec![] };
    let n = s.lock().silences;
    if n == 0 {
        return vec![];
    }
    vec![format!(
        "the SRT sender stopped sending {n} time(s) while its connection stayed open (no data packet for {SILENT_S:.0} s or more)"
    )]
}

/// The setup of an accepted connection.
pub fn link(conn: u64, settings: &ConnectionSettings, lab_latency_ms: u64, started_s: f64) -> SrtLink {
    SrtLink {
        conn,
        remote: settings.remote.to_string(),
        stream_id: settings.stream_id.clone().unwrap_or_default(),
        latency_ms: settings.recv_tsbpd_latency.as_millis() as u64,
        lab_latency_ms,
        peer_latency_ms: settings.send_tsbpd_latency.as_millis() as u64,
        handshake_rtt_ms: settings.rtt.as_secs_f64() * 1000.0,
        flow_window_pkts: settings.max_flow_size.0,
        max_packet_bytes: settings.max_packet_size.0,
        recv_buffer_pkts: settings.recv_buffer_size.0,
        too_late_drop: settings.too_late_packet_drop,
        encryption_bits: settings.cipher.as_ref().map(|c| c.key_settings.key_size.as_usize() as u32 * 8),
        started_s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(packets: u64) -> SocketStatistics {
        let mut s = SocketStatistics::new();
        s.rx_data = packets;
        s.rx_unique_data = packets;
        s
    }

    /// A sender that sends a few packets, then nothing while the connection stays open (an
    /// srt-tokio sender whose send buffer overflowed): said once, and cleared when data comes back.
    #[test]
    fn a_sender_gone_quiet_on_an_open_connection_is_said() {
        let mut st = SrtStats::default();
        st.begin(SrtLink::default());
        let mut said = vec![];
        for (t, n) in [(1.0, 20), (2.0, 20), (3.0, 20), (4.0, 20), (5.0, 20), (6.0, 40)] {
            said.extend(st.update(t, &at(n), 0).into_iter().map(|(l, m)| (t, l, m)));
        }
        let warns: Vec<_> = said.iter().filter(|(_, l, m)| *l == Level::Warn && m.contains("stopped sending")).collect();
        assert_eq!(warns.len(), 1, "{said:?}");
        assert_eq!(warns[0].0, 4.0, "3 s after the last data packet");
        assert!(warns[0].2.contains("after 20 packet(s)"), "{}", warns[0].2);
        assert!(said.iter().any(|(t, l, m)| *t == 6.0 && *l == Level::Info && m.contains("arriving again")));
        assert!(!st.silent_warning);
        assert_eq!(st.silences, 1);
    }

    /// Retransmissions arriving after their packet was played (the first copy got through) are
    /// late, not dropped; a packet given up on that arrives later is one of each.
    #[test]
    fn needless_retransmissions_are_not_drops() {
        let mut s = at(529);
        s.rx_unique_data = 499;
        s.rx_dropped_data = 30;
        let c = SrtCounters::from_socket(&s, 0);
        assert_eq!((c.dropped, c.late, c.missing), (0, 30, 0));
        // 2 given up on, one of which came in afterwards
        let mut s = at(501);
        s.rx_unique_data = 500;
        s.rx_dropped_data = 3;
        let c = SrtCounters::from_socket(&s, 0);
        assert_eq!((c.dropped, c.late, c.missing), (2, 1, 2));
    }

    #[test]
    fn a_connection_that_never_sends_data_is_said() {
        let mut st = SrtStats::default();
        st.begin(SrtLink::default());
        let said: Vec<_> = (1..=4).flat_map(|t| st.update(t as f64, &at(0), 0)).collect();
        assert!(said.iter().any(|(_, m)| m.contains("since it opened")), "{said:?}");
        assert!(st.silent_warning);
    }
}
