//! Wire types shared by the WASM client and the native server.
//!
//! The whole crate is `serde`-derive and value types — no I/O, no runtime
//! dependencies — so it compiles cleanly for both `wasm32-unknown-unknown`
//! and the host. All frames travel as JSON for v1; the tagged-enum derives
//! line up with `serde_json`'s `tag = "..."` representation directly.

use serde::{Deserialize, Serialize};

// --- Primitive identifiers --------------------------------------------------

/// Per-subscription identifier allocated by the client and echoed by the
/// server on every frame for routing. Reused across reconnects (the server
/// has forgotten the old connection so collisions are impossible).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SubId(pub u32);

// --- Timeframe --------------------------------------------------------------

/// Bar timeframe. Wire form is the Binance-style short string (`"1m"`,
/// `"5m"`, … `"1d"`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Timeframe {
    #[serde(rename = "1m")]
    M1,
    #[serde(rename = "5m")]
    M5,
    #[serde(rename = "15m")]
    M15,
    #[serde(rename = "30m")]
    M30,
    #[serde(rename = "1h")]
    H1,
    #[serde(rename = "2h")]
    H2,
    #[serde(rename = "4h")]
    H4,
    #[serde(rename = "6h")]
    H6,
    #[serde(rename = "1d")]
    D1,
}

impl Timeframe {
    /// All timeframes in display order. Drives both the chart selector and
    /// the server's combined-stream subscription set.
    pub const ALL: [Timeframe; 9] = [
        Timeframe::M1,
        Timeframe::M5,
        Timeframe::M15,
        Timeframe::M30,
        Timeframe::H1,
        Timeframe::H2,
        Timeframe::H4,
        Timeframe::H6,
        Timeframe::D1,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Timeframe::M1 => "1m",
            Timeframe::M5 => "5m",
            Timeframe::M15 => "15m",
            Timeframe::M30 => "30m",
            Timeframe::H1 => "1h",
            Timeframe::H2 => "2h",
            Timeframe::H4 => "4h",
            Timeframe::H6 => "6h",
            Timeframe::D1 => "1d",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Timeframe::ALL.into_iter().find(|tf| tf.as_str() == s)
    }

    /// Nominal bar span in milliseconds.
    pub fn duration_ms(self) -> i64 {
        match self {
            Timeframe::M1 => 60_000,
            Timeframe::M5 => 5 * 60_000,
            Timeframe::M15 => 15 * 60_000,
            Timeframe::M30 => 30 * 60_000,
            Timeframe::H1 => 60 * 60_000,
            Timeframe::H2 => 2 * 60 * 60_000,
            Timeframe::H4 => 4 * 60 * 60_000,
            Timeframe::H6 => 6 * 60 * 60_000,
            Timeframe::D1 => 24 * 60 * 60_000,
        }
    }
}

// --- Session ----------------------------------------------------------------

/// Trading session filter. Kept on the wire for forward-compat with equities
/// venues; for crypto both variants are equivalent and the server treats
/// them identically.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Session {
    Regular,
    Extended,
}

impl Session {
    pub const ALL: [Session; 2] = [Session::Regular, Session::Extended];

    pub fn as_str(self) -> &'static str {
        match self {
            Session::Regular => "regular",
            Session::Extended => "extended",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Session::ALL.into_iter().find(|sn| sn.as_str() == s)
    }
}

// --- Candle (wire-narrow OHLCV) ---------------------------------------------

/// Wire-narrow OHLCV bar. Timestamps are millis since the Unix epoch.
///
/// The `date` display string the client renders in axis labels is derived
/// client-side at deserialize time; the DB carries `quote_volume`,
/// `taker_buy_vol`, and `trades` columns the wire skips for v1 (see Q11).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Candle {
    pub open_time: i64,
    pub close_time: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

// --- Connection status ------------------------------------------------------

/// Health of the client↔server WebSocket from the client's perspective.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LiveStatus {
    Connecting,
    Connected,
    Reconnecting { attempts: u32 },
}

// --- Subscription channel ---------------------------------------------------

/// What kind of data a `Subscribe` op is asking for. Tagged enum on the wire
/// so adding new data kinds (trades, footprint, book) is purely additive on
/// both ends.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Channel {
    Candles { tf: Timeframe, session: Session },
    // Future variants (Q11):
    //   Trades,
    //   Footprint { tf: Timeframe, price_bucket: f64 },
    //   Book { depth: u16, throttle_ms: u16 },
}

// --- Client → server frames -------------------------------------------------

/// Frames the client sends to the server, tagged by `"op"`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ClientFrame {
    Subscribe {
        id: SubId,
        symbol: String,
        channel: Channel,
    },
    Unsubscribe {
        id: SubId,
    },
    HistoryPage {
        id: SubId,
        before_ms: i64,
        count: u32,
    },
    Ping {
        ts_ms: i64,
    },
}

// --- Server → client frames -------------------------------------------------

/// Frames the server sends to the client, tagged by `"type"`. `Snapshot`,
/// `Tick`, and `HistoryPage` carry a `SubId` so the client routes them to
/// the right per-subscription `BarStream`. `Resnap` requests the client
/// reset its buffer for a subscription before the next `Snapshot` arrives.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerFrame {
    Snapshot {
        id: SubId,
        candles: Vec<Candle>,
        /// Monotonic per-subscription cursor; the first `Tick` after this
        /// frame carries `v = server_v + 1`.
        server_v: u64,
    },
    Tick {
        id: SubId,
        candle: Candle,
        is_closed: bool,
        v: u64,
    },
    HistoryPage {
        id: SubId,
        candles: Vec<Candle>,
    },
    /// Server-detected gap on this subscription — client should reset the
    /// per-subscription state and await the next `Snapshot`.
    Resnap {
        id: SubId,
    },
    Status {
        state: LiveStatus,
    },
    Pong {
        ts_ms: i64,
    },
    Error {
        id: Option<SubId>,
        code: String,
        msg: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeframe_roundtrip() {
        for tf in Timeframe::ALL {
            let s = serde_json::to_string(&tf).unwrap();
            let back: Timeframe = serde_json::from_str(&s).unwrap();
            assert_eq!(tf, back);
            // Wire form matches `as_str()`.
            assert_eq!(s, format!("\"{}\"", tf.as_str()));
        }
    }

    #[test]
    fn subscribe_frame_shape() {
        let f = ClientFrame::Subscribe {
            id: SubId(7),
            symbol: "BTCUSDT".into(),
            channel: Channel::Candles {
                tf: Timeframe::M1,
                session: Session::Regular,
            },
        };
        let s = serde_json::to_string(&f).unwrap();
        assert!(s.contains("\"op\":\"subscribe\""));
        assert!(s.contains("\"kind\":\"candles\""));
        assert!(s.contains("\"tf\":\"1m\""));
        assert!(s.contains("\"id\":7"));
    }

    #[test]
    fn snapshot_frame_shape() {
        let f = ServerFrame::Snapshot {
            id: SubId(1),
            candles: vec![],
            server_v: 42,
        };
        let s = serde_json::to_string(&f).unwrap();
        assert!(s.contains("\"type\":\"snapshot\""));
        assert!(s.contains("\"server_v\":42"));
    }

    #[test]
    fn live_status_reconnecting() {
        let s =
            serde_json::to_string(&LiveStatus::Reconnecting { attempts: 3 }).unwrap();
        assert!(s.contains("\"state\":\"reconnecting\""));
        assert!(s.contains("\"attempts\":3"));
    }
}
