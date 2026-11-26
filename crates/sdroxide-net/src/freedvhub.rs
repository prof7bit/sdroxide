//! FreeDV Reporter on more than one server at once.
//!
//! RADE V2 testers gather on a server of their own, and the two do not share
//! anything. Rather than teach one session about two servers, the ordinary
//! session of [`crate::freedvreporter`] simply runs once per server. All of
//! them are told the same about our station ([`ReporterGroup`]), and what they
//! each learn is merged on the way out ([`Output`]), so nothing further along
//! can tell how many servers there are.
//!
//! The second server is for V2 testing, so we only report to it on V2. On V1
//! its session is a viewer: its stations are still listed, and it is told
//! nothing about us.
//!
//! Only entries from *different* servers are ever merged. One server listing a
//! callsign twice means two sessions, which may well be two radios.

use std::sync::{Arc, Mutex};

use sdroxide_types::{FreeDvReporterConfig, FreeDvStation, RadeVersion, Spot, SpotKind};

use crate::event::{EventTx, FeedTx, NetEvent};
use crate::freedvreporter::{DEFAULT_HOST, ReporterHandle};

/// How far apart two servers' ideas of a station's frequency may be for them
/// to be the same entry: one of them may have a retune before the other.
const SAME_STATION_HZ: u64 = 1_000;

/// Most servers a group holds: the configured one and the second.
const SERVERS: usize = 2;

/// What every server's session has last reported, and where the merge goes.
struct Hub {
    feed: FeedTx,
    events: EventTx,
    spots: [Vec<Spot>; SERVERS],
    stations: [Vec<FreeDvStation>; SERVERS],
    sent_stations: Vec<FreeDvStation>,
}

/// Where one server's session puts what it learns.
#[derive(Clone)]
pub(crate) struct Output {
    hub: Arc<Mutex<Hub>>,
    server: usize,
}

impl Output {
    /// The output of the first server's session.
    pub(crate) fn new(feed: FeedTx, events: EventTx) -> Output {
        let hub = Hub {
            feed,
            events,
            spots: Default::default(),
            stations: Default::default(),
            sent_stations: Vec::new(),
        };
        Output { hub: Arc::new(Mutex::new(hub)), server: 0 }
    }

    /// The output of the second server's session, merged with this one.
    fn second(&self) -> Output {
        Output { hub: self.hub.clone(), server: 1 }
    }

    /// Held while sending too, so that two sessions cannot deliver their merges
    /// out of order.
    fn hub(&self) -> std::sync::MutexGuard<'_, Hub> {
        self.hub.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// This server's whole spot set. Always passed on: a repeat is what keeps
    /// the spots from ageing out.
    pub(crate) fn spots(&self, spots: Vec<Spot>) {
        let mut hub = self.hub();
        hub.spots[self.server] = spots;
        let merged = merge_spots(&hub.spots[0], &hub.spots[1]);
        let _ = hub.feed.send((SpotKind::FreeDv, merged));
    }

    /// This server's whole station list.
    pub(crate) fn stations(&self, stations: Vec<FreeDvStation>) {
        let mut hub = self.hub();
        hub.stations[self.server] = stations;
        let merged = merge_stations(&hub.stations[0], &hub.stations[1]);
        if merged != hub.sent_stations {
            hub.sent_stations = merged.clone();
            let _ = hub.events.send(NetEvent::FreeDvStations(merged));
        }
    }

    /// A status line, saying which server it is about if not the first.
    pub(crate) fn status(&self, status: Option<String>) {
        let status = match self.server {
            0 => status,
            _ => status.map(|s| s.replacen("FreeDV Reporter:", "FreeDV Reporter (2nd server):", 1)),
        };
        let _ = self.hub().events.send(NetEvent::Status(status));
    }
}

/// For each entry of `second`, the entry of `first` that is the same station,
/// if any. Each of `first` is matched at most once, the closest in frequency.
fn pair<T>(
    first: &[T],
    second: &[T],
    distance: impl Fn(&T, &T) -> Option<u64>,
) -> Vec<Option<usize>> {
    let mut taken = vec![false; first.len()];
    second
        .iter()
        .map(|b| {
            let best = first
                .iter()
                .enumerate()
                .filter(|(i, _)| !taken[*i])
                .filter_map(|(i, a)| Some((distance(a, b)?, i)))
                .min();
            best.map(|(_, i)| {
                taken[i] = true;
                i
            })
        })
        .collect()
}

/// One list out of the two servers'. Where both know a station, anything
/// either of them says it is doing counts: they hear from the same program, so
/// they differ only while one is ahead of the other.
fn merge_stations(first: &[FreeDvStation], second: &[FreeDvStation]) -> Vec<FreeDvStation> {
    let matches = pair(first, second, |a, b| {
        let hz = a.freq_hz.abs_diff(b.freq_hz);
        (a.call == b.call && a.grid.eq_ignore_ascii_case(&b.grid) && hz <= SAME_STATION_HZ)
            .then_some(hz)
    });
    let mut out = first.to_vec();
    for (b, found) in second.iter().zip(matches) {
        let Some(i) = found else {
            out.push(b.clone());
            continue;
        };
        let a = &mut out[i];
        a.tx |= b.tx;
        a.rx_snr_db = a.rx_snr_db.or(b.rx_snr_db);
        a.lsb = a.lsb.or(b.lsb);
        for (mine, theirs) in [(&mut a.version, &b.version), (&mut a.message, &b.message)] {
            if mine.is_empty() {
                mine.clone_from(theirs);
            }
        }
    }
    // Stable, so that one call listed twice keeps its order.
    out.sort_by(|a, b| (&a.call, a.freq_hz).cmp(&(&b.call, b.freq_hz)));
    out
}

/// The spots of both servers, one marker per station.
fn merge_spots(first: &[Spot], second: &[Spot]) -> Vec<Spot> {
    let matches = pair(first, second, |a, b| {
        let hz = (a.freq_hz - b.freq_hz).abs() as u64;
        (a.call == b.call && hz <= SAME_STATION_HZ).then_some(hz)
    });
    let mut out = first.to_vec();
    for (b, found) in second.iter().zip(matches) {
        if let Some(i) = found {
            out[i].lsb = out[i].lsb.or(b.lsb);
        }
        match found {
            None => out.push(b.clone()),
            // Who heard them, and how well, is worth having from either.
            Some(i) if out[i].snr_db.is_none() && b.snr_db.is_some() => {
                out[i].snr_db = b.snr_db;
                out[i].spotter.clone_from(&b.spotter);
            }
            Some(_) => {}
        }
    }
    out
}

/// The second server's host, unless there is none to open: the field is empty,
/// or it names the first server again, where we must not sign in twice.
fn second_host(cfg: &FreeDvReporterConfig) -> Option<&str> {
    let second = cfg.second_host.trim();
    let first = match cfg.host.trim() {
        "" => DEFAULT_HOST,
        h => h,
    };
    (!second.is_empty() && !second.eq_ignore_ascii_case(first)).then_some(second)
}

/// The sessions of all servers, told the same things. Dropping it stops them.
pub(crate) struct ReporterGroup {
    sessions: Vec<ReporterHandle>,
}

impl ReporterGroup {
    /// See [`ReporterHandle::connect`].
    pub(crate) fn connect(
        cfg: FreeDvReporterConfig,
        call: String,
        grid: String,
        feed: FeedTx,
        events: EventTx,
    ) -> ReporterGroup {
        let out = Output::new(feed, events);
        let mut sessions = Vec::with_capacity(SERVERS);
        if let Some(host) = second_host(&cfg) {
            // The public servers of both kinds listen on the default port.
            let second = FreeDvReporterConfig { host: host.to_string(), port: 80, ..cfg.clone() };
            sessions.push(ReporterHandle::connect(
                second,
                call.clone(),
                grid.clone(),
                out.second(),
                Some(RadeVersion::V2),
            ));
        }
        sessions.push(ReporterHandle::connect(cfg, call, grid, out, None));
        ReporterGroup { sessions }
    }

    fn each(&self, f: impl Fn(&ReporterHandle)) {
        self.sessions.iter().for_each(f);
    }

    pub(crate) fn set_freq(&self, hz: u64) {
        self.each(|s| s.set_freq(hz));
    }
    pub(crate) fn set_tx(&self, on: bool) {
        self.each(|s| s.set_tx(on));
    }
    pub(crate) fn set_sideband(&self, lower: bool) {
        self.each(|s| s.set_sideband(lower));
    }
    pub(crate) fn set_version(&self, version: RadeVersion) {
        self.each(|s| s.set_version(version));
    }
    pub(crate) fn set_visible(&self, visible: bool) {
        self.each(|s| s.set_visible(visible));
    }
    pub(crate) fn set_message(&self, message: String) {
        self.each(|s| s.set_message(message.clone()));
    }
    pub(crate) fn rx_report(&self, call: String, snr: i32) {
        self.each(|s| s.rx_report(call.clone(), snr));
    }
    pub(crate) fn rx_presence(&self, snr: i32) {
        self.each(|s| s.rx_presence(snr));
    }
}

impl Drop for ReporterGroup {
    fn drop(&mut self) {
        // All told first, so that the waits overlap: a server that is gone can
        // hold its session in a connection attempt for several seconds.
        self.each(ReporterHandle::stop);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn station(call: &str, grid: &str, freq_hz: u64) -> FreeDvStation {
        FreeDvStation {
            call: call.into(),
            freq_hz,
            tx: false,
            rx_snr_db: None,
            grid: grid.into(),
            version: String::new(),
            message: String::new(),
            lsb: None,
        }
    }

    #[test]
    fn a_station_on_both_servers_is_listed_once() {
        let a = station("OE2RXN", "JN67pi", 7_177_000);
        let mut b = station("OE2RXN", "JN67PI", 7_177_400);
        b.tx = true;
        b.rx_snr_db = Some(4);
        b.message = "QRV".into();
        let only_second = station("DG4SDX", "JN48RW", 7_177_000);

        let merged = merge_stations(std::slice::from_ref(&a), &[b, only_second]);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].call, "DG4SDX");
        let both = &merged[1];
        assert_eq!((both.freq_hz, both.tx, both.rx_snr_db), (7_177_000, true, Some(4)));
        assert_eq!(both.message, "QRV");
    }

    #[test]
    fn different_stations_stay_apart() {
        let a = station("OE2RXN", "JN67pi", 7_177_000);
        // Another band, another place: both are other stations.
        let elsewhere =
            [station("OE2RXN", "JN67pi", 14_236_000), station("OE2RXN", "JN48", 7_177_000)];
        assert_eq!(merge_stations(std::slice::from_ref(&a), &elsewhere).len(), 3);
        // Twice on one server is two sessions, and one on the other pairs
        // with only one of them.
        let twice = [a.clone(), a.clone()];
        assert_eq!(merge_stations(&twice, &[]).len(), 2);
        assert_eq!(merge_stations(&twice, std::slice::from_ref(&a)).len(), 2);
    }

    #[test]
    fn one_session_leaving_takes_only_its_own_stations() {
        let (feed, _feed_rx) = crossbeam_channel::unbounded();
        let (events, rx) = crossbeam_channel::unbounded();
        let first = Output::new(feed, events);
        let second = first.second();
        let latest = || match rx.try_iter().last() {
            Some(NetEvent::FreeDvStations(s)) => Some(s.len()),
            _ => None,
        };
        first.stations(vec![station("DL1AAA", "JO31", 7_177_000)]);
        second.stations(vec![station("DL2BBB", "JO31", 7_177_000)]);
        assert_eq!(latest(), Some(2));
        second.stations(Vec::new());
        assert_eq!(latest(), Some(1));
        second.stations(Vec::new());
        assert_eq!(latest(), None, "nothing changed, nothing sent");
    }

    #[test]
    fn the_second_server_can_be_switched_off_and_is_never_the_first() {
        let mut cfg = FreeDvReporterConfig::default();
        assert_eq!(second_host(&cfg), Some("reporter-radev2.k6aq.net"));
        cfg.second_host = "  ".into();
        assert_eq!(second_host(&cfg), None);
        cfg.second_host = "QSO.freedv.org".into();
        cfg.host = String::new();
        assert_eq!(second_host(&cfg), None);
    }
}
