//! Live market-data service (stub).
//!
//! Public types and function signatures from the original centoflow-backed
//! implementation are preserved verbatim so the chart panel compiles and the
//! `MarketDataServiceHandle` global is available. All bodies are no-ops:
//! `ensure` returns a handle pointing at an empty buffer, no ticks ever
//! arrive, status stays `Connecting` forever. A real BTC backend will fill
//! these in.

use std::collections::HashMap;

use chrono::{Local, TimeZone as _};
use futures::channel::mpsc::{UnboundedSender, unbounded};
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, SharedString};

/// A chart timeframe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Timeframe {
    M1,
    M5,
    M15,
    M30,
    H1,
    H2,
    H4,
    H6,
    D1,
}

/// Timeframe shown by default on a freshly-opened chart.
pub const DEFAULT_TIMEFRAME: Timeframe = Timeframe::M5;

impl Timeframe {
    /// All timeframes in display order — drives the chart's tf selector.
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

    /// Nominal bar span in milliseconds. Used by the chart x-axis step picker.
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

/// Trading session filter. Kept on the API for compatibility with the chart
/// panel's selector; for crypto both variants are effectively the same.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Session {
    Regular,
    Extended,
}

pub const DEFAULT_SESSION: Session = Session::Regular;

impl Session {
    pub const ALL: [Session; 2] = [Session::Regular, Session::Extended];

    pub fn as_str(self) -> &'static str {
        match self {
            Session::Regular => "regular",
            Session::Extended => "extended",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Session::Regular => "RTH",
            Session::Extended => "ETH",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Session::ALL.into_iter().find(|sn| sn.as_str() == s)
    }
}

/// A single OHLCV bar.
#[derive(Clone, Debug)]
pub struct Candle {
    pub open_time: i64,
    pub close_time: i64,
    pub date: SharedString,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
    pub vwap: Option<f64>,
    pub trades: Option<i32>,
}

impl Candle {
    pub fn new(
        open_time: i64,
        close_time: i64,
        open: f64,
        high: f64,
        low: f64,
        close: f64,
        volume: f64,
    ) -> Self {
        Self::new_full(open_time, close_time, open, high, low, close, volume, None, None)
    }

    pub fn new_full(
        open_time: i64,
        close_time: i64,
        open: f64,
        high: f64,
        low: f64,
        close: f64,
        volume: f64,
        vwap: Option<f64>,
        trades: Option<i32>,
    ) -> Self {
        let date = Local
            .timestamp_millis_opt(open_time)
            .single()
            .map(|dt| dt.format("%b %d %H:%M").to_string())
            .unwrap_or_default();
        Self {
            open_time,
            close_time,
            date: date.into(),
            open,
            high,
            low,
            close,
            volume,
            vwap,
            trades,
        }
    }
}

/// Whether a display symbol has a live feed. The stub answers `false` for
/// everything, which lets the chart's fallback paths render without ever
/// expecting WS data.
pub fn is_live(_display_symbol: &str) -> bool {
    false
}

#[derive(Clone, Debug, PartialEq)]
pub enum LiveStatus {
    Connecting,
    Connected,
    Reconnecting { attempts: u32 },
}

#[derive(Clone, Debug)]
pub enum KlineEvent {
    Tick {
        symbol: SharedString,
        tf: Timeframe,
        session: Session,
        candle: Candle,
        is_closed: bool,
    },
    Resnap {
        symbol: SharedString,
        tf: Timeframe,
        session: Session,
    },
    Prepended {
        symbol: SharedString,
        tf: Timeframe,
        session: Session,
        added: usize,
    },
    HistoryCapped {
        symbol: SharedString,
        tf: Timeframe,
        session: Session,
    },
    StatusChanged {
        symbol: SharedString,
        tf: Timeframe,
        session: Session,
        status: LiveStatus,
    },
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct SubKey {
    pub(crate) symbol: String,
    pub(crate) tf: Timeframe,
    pub(crate) session: Session,
}

impl SubKey {
    fn new(symbol: &str, tf: Timeframe, session: Session) -> Self {
        Self {
            symbol: symbol.to_string(),
            tf,
            session,
        }
    }
}

pub struct MarketDataService {
    candles: HashMap<SubKey, Vec<Candle>>,
    release_tx: UnboundedSender<SubKey>,
}

impl EventEmitter<KlineEvent> for MarketDataService {}

impl MarketDataService {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        let (release_tx, _release_rx) = unbounded::<SubKey>();
        Self {
            candles: HashMap::new(),
            release_tx,
        }
    }

    /// Register interest in `(symbol, tf, session)`. Returns a refcounted
    /// handle; in the stub, no backfill is fetched and no ticks ever arrive,
    /// so the snapshot stays empty forever.
    pub fn ensure(
        &mut self,
        symbol: &str,
        tf: Timeframe,
        session: Session,
        _cx: &mut Context<Self>,
    ) -> SubscriptionHandle {
        let key = SubKey::new(symbol, tf, session);
        self.candles.entry(key.clone()).or_default();
        SubscriptionHandle {
            key,
            release_tx: self.release_tx.clone(),
        }
    }

    pub fn reconnect_all(&mut self, _cx: &mut Context<Self>) {}

    pub fn snapshot(&self, symbol: &str, tf: Timeframe, session: Session) -> Option<&[Candle]> {
        self.candles
            .get(&SubKey::new(symbol, tf, session))
            .map(|v| v.as_slice())
    }

    pub fn status(&self, _symbol: &str, _tf: Timeframe, _session: Session) -> LiveStatus {
        LiveStatus::Connecting
    }

    pub fn overall_status(&self) -> LiveStatus {
        LiveStatus::Connecting
    }

    pub fn last_message_ms(&self) -> Option<i64> {
        None
    }

    pub fn load_older(
        &mut self,
        _symbol: &str,
        _tf: Timeframe,
        _session: Session,
        _cx: &mut Context<Self>,
    ) {
    }
}

#[derive(Clone)]
pub struct MarketDataServiceHandle(pub Entity<MarketDataService>);
impl Global for MarketDataServiceHandle {}

pub fn init(cx: &mut App) {
    let entity = cx.new(MarketDataService::new);
    cx.set_global(MarketDataServiceHandle(entity));
}

/// Handle to a live `(symbol, tf, session)` subscription. While at least one
/// handle exists the service keeps the entry registered; the stub never
/// fetches or streams anything.
pub struct SubscriptionHandle {
    #[allow(dead_code)]
    key: SubKey,
    #[allow(dead_code)]
    release_tx: UnboundedSender<SubKey>,
}
