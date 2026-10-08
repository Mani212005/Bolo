//! Live daemon event stream. A client sends `subscribe` on the daemon socket
//! and then receives one JSON object per line: the current state (`hello`),
//! phase changes, mic levels while recording, and the outcome of each
//! dictation. The macOS pill renders it; `bolo events` prints it.
//!
//! Producers (audio thread, phase setter, finalize) only push into a channel.
//! A dedicated hub thread owns the subscribers and writes with a short
//! timeout, dropping any client that errors or stalls, so a stuck renderer can
//! never delay audio capture.
//!
//! The hub also knows the live pill settings (they change without a daemon
//! restart) and whether a pill renderer is connected, which the daemon uses to
//! decide whether its state banners are redundant.

use crate::config::{PillConfig, PillStyle};
use crate::daemon::Phase;
use crossbeam_channel::{Receiver, Sender};
use serde_json::{json, Value};
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Protocol version, sent in `hello`.
pub const PROTOCOL_VERSION: u32 = 1;

/// How long a subscriber may block a write before it is dropped.
const WRITE_TIMEOUT: Duration = Duration::from_millis(250);

/// The quietest and loudest dBFS values the level meter distinguishes.
const FLOOR_DBFS: f32 = -60.0;
const CEIL_DBFS: f32 = -10.0;

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Phase(Phase),
    /// Mic level 0..1 for one VAD chunk, and whether the VAD heard speech.
    Level {
        rms: f32,
        speech: bool,
    },
    /// How a dictation ended. `detail` refines `kind`; `chars` is the pasted length.
    Outcome {
        kind: &'static str,
        detail: &'static str,
        chars: Option<usize>,
    },
    /// The pill settings changed (dashboard, `bolo pill-style`, or the pill's own menu).
    Config(PillConfig),
}

impl Event {
    pub fn to_json(&self) -> Value {
        match self {
            Event::Phase(phase) => json!({ "type": "phase", "phase": phase.as_str() }),
            Event::Level { rms, speech } => json!({
                "type": "level",
                // Three decimals keep the ~31 lines per second small.
                "rms": (f64::from(*rms) * 1000.0).round() / 1000.0,
                "speech": speech,
            }),
            Event::Outcome {
                kind,
                detail,
                chars,
            } => {
                let mut v = json!({ "type": "outcome", "kind": kind, "detail": detail });
                if let Some(chars) = chars {
                    v["chars"] = json!(chars);
                }
                v
            }
            Event::Config(pill) => json!({
                "type": "config",
                "style": pill.style.as_str(),
                "show_idle": pill.show_when_idle,
            }),
        }
    }
}

/// What a new subscriber learns first: the current phase and pill settings.
fn hello(phase: Phase, pill: &PillConfig) -> Value {
    json!({
        "v": PROTOCOL_VERSION,
        "type": "hello",
        "phase": phase.as_str(),
        "style": pill.style.as_str(),
        "show_idle": pill.show_when_idle,
    })
}

/// Maps a chunk of 16-bit samples to a 0..1 meter value: RMS in dBFS,
/// -60 dBFS (and below) is 0, -10 dBFS (and above) is 1.
pub fn level_from_chunk(chunk: &[i16]) -> f32 {
    if chunk.is_empty() {
        return 0.0;
    }
    let mean_square = chunk
        .iter()
        .map(|&s| {
            let s = f64::from(s) / 32768.0;
            s * s
        })
        .sum::<f64>()
        / chunk.len() as f64;
    if mean_square <= 0.0 {
        return 0.0;
    }
    let dbfs = (10.0 * mean_square.log10()) as f32;
    ((dbfs - FLOOR_DBFS) / (CEIL_DBFS - FLOOR_DBFS)).clamp(0.0, 1.0)
}

enum HubMsg {
    Event(Event),
    /// A new connection; `pill` marks the on-screen renderer (`subscribe pill`).
    Subscribe {
        stream: UnixStream,
        pill: bool,
    },
}

/// Cloneable handle to the broadcaster thread. `EventHub::default()` is a
/// handle that discards everything, for code paths and tests with no stream.
#[derive(Clone, Default)]
pub struct EventHub {
    tx: Option<Sender<HubMsg>>,
    subscribers: Arc<AtomicUsize>,
    /// How many of the subscribers are the pill renderer.
    pill_clients: Arc<AtomicUsize>,
    /// The live pill settings.
    pill: Arc<Mutex<PillConfig>>,
}

impl EventHub {
    pub fn spawn(pill: PillConfig) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        let hub = Self {
            tx: Some(tx),
            pill: Arc::new(Mutex::new(pill)),
            ..Self::default()
        };
        let worker = hub.clone();
        std::thread::spawn(move || run_hub(rx, worker));
        hub
    }

    /// The pill settings as they are now.
    pub fn pill(&self) -> PillConfig {
        self.pill.lock().unwrap().clone()
    }

    /// Applies new pill settings and tells every subscriber. Returns false when
    /// nothing changed.
    pub fn set_pill(&self, pill: PillConfig) -> bool {
        {
            let mut current = self.pill.lock().unwrap();
            if *current == pill {
                return false;
            }
            *current = pill.clone();
        }
        self.publish(Event::Config(pill));
        true
    }

    /// True when a pill renderer is connected and its style draws something.
    /// The daemon's "Listening / Paused / Transcribing" banners are redundant
    /// then; without it (plain `cargo build`, Linux, Hidden style) they are the
    /// only live cue and stay.
    pub fn pill_visible(&self) -> bool {
        self.pill.lock().unwrap().style != PillStyle::Hidden
            && self.pill_clients.load(Ordering::Relaxed) > 0
    }

    pub fn publish(&self, event: Event) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(HubMsg::Event(event));
        }
    }

    /// Level events are the only high-rate ones; skip them while nobody listens.
    pub fn publish_level(&self, rms: f32, speech: bool) {
        if self.subscribers.load(Ordering::Relaxed) > 0 {
            self.publish(Event::Level { rms, speech });
        }
    }

    /// Hands a connection to the hub; it receives `hello` and then every event.
    pub fn subscribe(&self, stream: UnixStream) {
        self.send_subscribe(stream, false);
    }

    /// Like `subscribe`, for the on-screen pill (`subscribe pill`): the hub
    /// counts it so the daemon knows the pill is really there.
    pub fn subscribe_pill(&self, stream: UnixStream) {
        self.send_subscribe(stream, true);
    }

    fn send_subscribe(&self, stream: UnixStream, pill: bool) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(HubMsg::Subscribe { stream, pill });
        }
    }
}

fn run_hub(rx: Receiver<HubMsg>, hub: EventHub) {
    let mut phase = Phase::Idle;
    // (connection, is the pill renderer)
    let mut subscribers: Vec<(UnixStream, bool)> = Vec::new();
    let publish_counts = |subscribers: &[(UnixStream, bool)]| {
        hub.subscribers.store(subscribers.len(), Ordering::Relaxed);
        hub.pill_clients.store(
            subscribers.iter().filter(|(_, pill)| *pill).count(),
            Ordering::Relaxed,
        );
    };
    for msg in rx.iter() {
        let line = match msg {
            HubMsg::Subscribe { stream, pill } => {
                let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
                subscribers.push((stream, pill));
                // Only the new client gets the snapshot.
                let snapshot = hello(phase, &hub.pill()).to_string();
                let last = subscribers.len() - 1;
                if !write_line(&mut subscribers[last].0, &snapshot) {
                    subscribers.pop();
                }
                publish_counts(&subscribers);
                continue;
            }
            HubMsg::Event(event) => {
                if let Event::Phase(p) = &event {
                    phase = *p;
                }
                event.to_json().to_string()
            }
        };
        subscribers.retain_mut(|(stream, _)| write_line(stream, &line));
        publish_counts(&subscribers);
    }
}

/// Writes one line; false when the client is gone or too slow.
fn write_line(stream: &mut UnixStream, line: &str) -> bool {
    stream
        .write_all(format!("{line}\n").as_bytes())
        .and_then(|()| stream.flush())
        .is_ok()
}

/// Helpers for tests in other modules that watch the stream.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::io::{BufRead, BufReader};

    /// A subscriber connection: the hub-side end to hand over, and a channel fed
    /// by a thread that reads the client side continuously (like a real pill).
    pub(crate) fn connection() -> (UnixStream, Receiver<Value>) {
        let (hub_end, client_end) = UnixStream::pair().unwrap();
        let (tx, rx) = crossbeam_channel::unbounded();
        std::thread::spawn(move || {
            for line in BufReader::new(client_end).lines() {
                let Ok(line) = line else { break };
                let _ = tx.send(serde_json::from_str(&line).unwrap());
            }
        });
        (hub_end, rx)
    }

    pub(crate) fn next(rx: &Receiver<Value>) -> Value {
        rx.recv_timeout(Duration::from_secs(5)).unwrap()
    }

    /// Subscribes to `hub` and swallows the hello.
    pub(crate) fn subscribe(hub: &EventHub) -> Receiver<Value> {
        let (hub_end, rx) = connection();
        hub.subscribe(hub_end);
        assert_eq!(next(&rx)["type"], "hello");
        rx
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{connection, next};
    use super::*;

    fn pill() -> PillConfig {
        PillConfig::default()
    }

    fn sine(amplitude: f64, len: usize) -> Vec<i16> {
        (0..len)
            .map(|i| (amplitude * (i as f64 * 0.3).sin() * 32767.0) as i16)
            .collect()
    }

    #[test]
    fn level_maps_dbfs_to_the_unit_range() {
        assert_eq!(level_from_chunk(&[0; 512]), 0.0);
        assert_eq!(level_from_chunk(&[]), 0.0);
        // A full-scale sine is about -3 dBFS, above the ceiling.
        assert_eq!(level_from_chunk(&sine(1.0, 512)), 1.0);
        // RMS of a sine is amplitude / sqrt(2): this one sits at about -35 dBFS.
        let amplitude = 10f64.powf(-35.0 / 20.0) * std::f64::consts::SQRT_2;
        let mid = level_from_chunk(&sine(amplitude, 4096));
        assert!((mid - 0.5).abs() < 0.03, "got {mid}");
        // Below the floor stays at zero.
        assert_eq!(level_from_chunk(&sine(0.0005, 512)), 0.0);
    }

    #[test]
    fn events_serialise_to_the_documented_json() {
        assert_eq!(
            Event::Phase(Phase::Processing).to_json(),
            json!({ "type": "phase", "phase": "processing" })
        );
        assert_eq!(
            Event::Level {
                rms: 0.123_456,
                speech: true
            }
            .to_json(),
            json!({ "type": "level", "rms": 0.123, "speech": true })
        );
        assert_eq!(
            Event::Outcome {
                kind: "done",
                detail: "pasted",
                chars: Some(182)
            }
            .to_json(),
            json!({ "type": "outcome", "kind": "done", "detail": "pasted", "chars": 182 })
        );
        assert_eq!(
            Event::Outcome {
                kind: "no-speech",
                detail: "",
                chars: None
            }
            .to_json(),
            json!({ "type": "outcome", "kind": "no-speech", "detail": "" })
        );
    }

    #[test]
    fn hello_carries_the_version_phase_and_pill_settings() {
        let v = hello(Phase::Recording, &pill());
        assert_eq!(
            v,
            json!({ "v": 1, "type": "hello", "phase": "recording", "style": "small", "show_idle": true })
        );
        // The wire form is one JSON object per line.
        assert!(!v.to_string().contains('\n'));
    }

    #[test]
    fn subscriber_gets_a_hello_with_the_current_phase_then_live_events() {
        let hub = EventHub::spawn(pill());
        hub.publish(Event::Phase(Phase::Recording));
        let (hub_end, client) = connection();
        hub.subscribe(hub_end);
        let first = next(&client);
        assert_eq!(first["type"], "hello");
        assert_eq!(first["v"], 1);
        assert_eq!(first["phase"], "recording");

        hub.publish_level(0.4, true);
        hub.publish(Event::Phase(Phase::Processing));
        assert_eq!(next(&client)["type"], "level");
        assert_eq!(
            next(&client),
            json!({ "type": "phase", "phase": "processing" })
        );
    }

    #[test]
    fn levels_are_not_sent_while_nobody_is_subscribed() {
        let hub = EventHub::spawn(pill());
        hub.publish_level(0.9, true); // dropped: no subscriber yet
        let (hub_end, client) = connection();
        hub.subscribe(hub_end);
        assert_eq!(next(&client)["type"], "hello");
        hub.publish(Event::Phase(Phase::Idle));
        assert_eq!(next(&client)["type"], "phase");
    }

    #[test]
    fn closed_subscriber_is_dropped_without_blocking_the_producer() {
        let hub = EventHub::spawn(pill());
        let (dead_end, dead_client) = UnixStream::pair().unwrap();
        hub.subscribe(dead_end);
        drop(dead_client);
        let (live_end, live) = connection();
        hub.subscribe(live_end);
        assert_eq!(next(&live)["type"], "hello");

        let started = std::time::Instant::now();
        for _ in 0..2000 {
            hub.publish(Event::Level {
                rms: 0.5,
                speech: false,
            });
        }
        // Producers only push into a channel, whatever the subscribers do.
        assert!(started.elapsed() < Duration::from_millis(200));
        hub.publish(Event::Phase(Phase::Idle));
        loop {
            if next(&live)["type"] == "phase" {
                break;
            }
        }
        assert_eq!(hub.subscribers.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn stalled_subscriber_is_dropped_and_others_keep_receiving() {
        let hub = EventHub::spawn(pill());
        // Never read from this one: its socket buffer fills and the write times out.
        let (stalled_end, _stalled_client) = UnixStream::pair().unwrap();
        hub.subscribe(stalled_end);
        let (live_end, live) = connection();
        hub.subscribe(live_end);
        assert_eq!(next(&live)["type"], "hello");

        let started = std::time::Instant::now();
        for _ in 0..20_000 {
            hub.publish(Event::Outcome {
                kind: "done",
                detail: "pasted",
                chars: Some(1),
            });
        }
        assert!(started.elapsed() < Duration::from_secs(1));
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while hub.subscribers.load(Ordering::Relaxed) != 1 {
            assert!(std::time::Instant::now() < deadline, "stalled client kept");
            std::thread::sleep(Duration::from_millis(20));
        }
        // The live client still gets what comes next.
        hub.publish(Event::Phase(Phase::Idle));
        loop {
            if next(&live)["type"] == "phase" {
                break;
            }
        }
    }

    #[test]
    fn config_event_and_hello_follow_the_live_pill_settings() {
        use crate::config::PillStyle;
        let large = PillConfig {
            style: PillStyle::Large,
            show_when_idle: false,
        };
        assert_eq!(
            Event::Config(large.clone()).to_json(),
            json!({ "type": "config", "style": "large", "show_idle": false })
        );

        let hub = EventHub::spawn(pill());
        assert!(hub.set_pill(large.clone()));
        assert!(!hub.set_pill(large.clone()), "no change, no event");
        // A client that connects after the change learns it from hello.
        let (hub_end, client) = connection();
        hub.subscribe(hub_end);
        let first = next(&client);
        assert_eq!(first["style"], "large");
        assert_eq!(first["show_idle"], false);
        assert_eq!(hub.pill(), large);
    }

    #[test]
    fn pill_visible_needs_a_connected_pill_with_a_style_that_draws() {
        use crate::config::PillStyle;
        let hub = EventHub::spawn(pill());
        assert!(!hub.pill_visible(), "nothing connected");
        let (plain_end, _plain) = connection();
        hub.subscribe(plain_end);
        let (pill_end, _pill_client) = connection();
        hub.subscribe_pill(pill_end);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !hub.pill_visible() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        hub.set_pill(PillConfig {
            style: PillStyle::Hidden,
            ..pill()
        });
        assert!(!hub.pill_visible(), "hidden style draws nothing");
    }

    #[test]
    fn default_hub_discards_everything() {
        let hub = EventHub::default();
        hub.publish(Event::Phase(Phase::Recording));
        hub.publish_level(0.5, true);
        let (hub_end, _client) = connection();
        hub.subscribe(hub_end);
    }
}
