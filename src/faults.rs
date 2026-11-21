//! Fault injection: what the lab can do to a stream to test a sender's recovery without anyone
//! touching their Wi-Fi. Faults belong to a stream key (or to `*`, every stream).

use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, Default)]
pub struct Timed<T: Copy> {
    pub value: T,
    pub until: Option<Instant>,
}

impl<T: Copy + Default + PartialEq> Timed<T> {
    pub fn current(&self, now: Instant) -> T {
        match self.until {
            Some(u) if now >= u => T::default(),
            _ => self.value,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Faults {
    pub refuse_until: Option<Instant>,
    pub freeze_until: Option<Instant>,
    pub throttle_kbps: Timed<u32>,
    pub latency_ms: Timed<u32>,
    pub loss_pct: Timed<f32>,
}

/// What to do, as the API and the CLI say it.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct FaultRequest {
    /// Drop the connection now (the sender reconnects by itself, if it does).
    pub drop: bool,
    /// Refuse new connections for this many seconds (and drop the current one).
    pub refuse_s: Option<f64>,
    /// Stop reading for this many seconds: a dead network that never says so.
    pub freeze_s: Option<f64>,
    /// Limit what the lab accepts to this many kbps (0 removes the limit).
    pub throttle_kbps: Option<u32>,
    /// Delay everything by this many ms, both ways (0 removes it).
    pub latency_ms: Option<u32>,
    /// Lose this percentage of packets (SRT; 0 removes it).
    pub loss_pct: Option<f32>,
    /// How long throttle, latency and loss last (default: until cleared).
    pub for_s: Option<f64>,
    /// Remove every fault.
    pub clear: bool,
}

impl Faults {
    pub fn refusing(&self, now: Instant) -> bool {
        self.refuse_until.is_some_and(|u| now < u)
    }

    pub fn frozen(&self, now: Instant) -> bool {
        self.freeze_until.is_some_and(|u| now < u)
    }

    /// Applies a request; says what it did, for the event log. The caller handles `drop`.
    pub fn apply(&mut self, r: &FaultRequest, now: Instant) -> Vec<String> {
        let mut said = vec![];
        let until = r.for_s.filter(|s| *s > 0.0).map(|s| now + Duration::from_secs_f64(s));
        let lasting = until.map_or(String::new(), |_| format!(" for {} s", r.for_s.unwrap_or(0.0)));
        if r.clear {
            *self = Faults::default();
            said.push("all faults cleared".into());
        }
        if let Some(s) = r.refuse_s {
            self.refuse_until = (s > 0.0).then(|| now + Duration::from_secs_f64(s));
            said.push(if s > 0.0 { format!("refusing new connections for {s} s") } else { "no longer refusing".into() });
        }
        if let Some(s) = r.freeze_s {
            self.freeze_until = (s > 0.0).then(|| now + Duration::from_secs_f64(s));
            said.push(if s > 0.0 { format!("reading frozen for {s} s") } else { "unfrozen".into() });
        }
        if let Some(k) = r.throttle_kbps {
            self.throttle_kbps = Timed { value: k, until };
            said.push(if k > 0 { format!("throttled to {k} kbps{lasting}") } else { "throttle removed".into() });
        }
        if let Some(ms) = r.latency_ms {
            self.latency_ms = Timed { value: ms, until };
            said.push(if ms > 0 { format!("{ms} ms added latency{lasting}") } else { "latency removed".into() });
        }
        if let Some(p) = r.loss_pct {
            self.loss_pct = Timed { value: p, until };
            said.push(if p > 0.0 { format!("{p}% packet loss{lasting}") } else { "packet loss removed".into() });
        }
        if r.drop {
            said.push("connection dropped".into());
        }
        said
    }

    pub fn describe(&self, now: Instant) -> Value {
        let left = |u: Option<Instant>| u.filter(|u| now < *u).map(|u| (u - now).as_secs_f64());
        json!({
            "refuse_left_s": left(self.refuse_until),
            "freeze_left_s": left(self.freeze_until),
            "throttle_kbps": self.throttle_kbps.current(now),
            "latency_ms": self.latency_ms.current(now),
            "loss_pct": self.loss_pct.current(now),
        })
    }
}

/// The faults in force on a connection: the stream's and the global ones together.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Effective {
    pub frozen: bool,
    pub throttle_kbps: u32,
    pub latency_ms: u32,
    pub loss_pct: f32,
}

pub fn merge(a: &Faults, b: &Faults, now: Instant) -> Effective {
    let (ta, tb) = (a.throttle_kbps.current(now), b.throttle_kbps.current(now));
    Effective {
        frozen: a.frozen(now) || b.frozen(now),
        throttle_kbps: match (ta, tb) {
            (0, x) | (x, 0) => x,
            (x, y) => x.min(y),
        },
        latency_ms: a.latency_ms.current(now) + b.latency_ms.current(now),
        loss_pct: a.loss_pct.current(now).max(b.loss_pct.current(now)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn faults_expire_and_merge() {
        let now = Instant::now();
        let mut a = Faults::default();
        let said = a.apply(&FaultRequest { throttle_kbps: Some(500), for_s: Some(5.0), refuse_s: Some(2.0), ..Default::default() }, now);
        assert_eq!(said.len(), 2);
        assert!(a.refusing(now) && !a.refusing(now + Duration::from_secs(3)));
        assert_eq!(a.throttle_kbps.current(now), 500);
        assert_eq!(a.throttle_kbps.current(now + Duration::from_secs(6)), 0);
        let mut b = Faults::default();
        b.apply(&FaultRequest { throttle_kbps: Some(300), latency_ms: Some(80), ..Default::default() }, now);
        let e = merge(&a, &b, now);
        assert_eq!((e.throttle_kbps, e.latency_ms), (300, 80));
        a.apply(&FaultRequest { clear: true, ..Default::default() }, now);
        assert_eq!(merge(&a, &Faults::default(), now), Effective::default());
    }
}
