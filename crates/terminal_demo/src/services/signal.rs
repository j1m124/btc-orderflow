//! Signal engine — exposes scored stock signals to the Signal panel. The
//! current implementation returns a hardcoded list shaped to match what a
//! real engine would emit (ticker + score + setup + reason + direction +
//! timeframe). When the real engine lands, only `SignalService::seed` needs
//! to change — panels consume `signals()` regardless of source.

use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, SharedString};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalDirection {
    Long,
    Short,
}

impl SignalDirection {
    pub fn label(self) -> &'static str {
        match self {
            SignalDirection::Long => "LONG",
            SignalDirection::Short => "SHORT",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Signal {
    pub ticker: SharedString,
    pub score: u8,
    pub setup: SharedString,
    pub reason: SharedString,
    pub direction: SignalDirection,
    pub timeframe: SharedString,
}

#[derive(Clone, Debug)]
pub enum SignalEvent {
    /// New signal list available.
    Updated,
    /// User changed which signal is focused.
    SelectionChanged,
}

pub struct SignalService {
    signals: Vec<Signal>,
    /// Ticker of the currently-focused signal, if any. Drives the
    /// SignalDetail panel. Defaults to the first signal in the seed list
    /// so the detail pane has something to show on first open.
    selected: Option<SharedString>,
}

impl EventEmitter<SignalEvent> for SignalService {}

impl SignalService {
    fn new(_cx: &mut Context<Self>) -> Self {
        let signals = seed_signals();
        let selected = signals.first().map(|s| s.ticker.clone());
        Self { signals, selected }
    }

    pub fn signals(&self) -> &[Signal] {
        &self.signals
    }

    /// Ticker of the focused signal, or `None` when the list is empty.
    pub fn selected(&self) -> Option<&SharedString> {
        self.selected.as_ref()
    }

    /// Look up the focused `Signal` by joining `selected` with the list.
    pub fn selected_signal(&self) -> Option<&Signal> {
        let ticker = self.selected.as_ref()?;
        self.signals.iter().find(|s| s.ticker == *ticker)
    }

    /// Set the focused ticker. No-op if `ticker` doesn't match a signal in
    /// the current list. Emits `SelectionChanged` so the detail panel
    /// repaints.
    pub fn select(&mut self, ticker: &str, cx: &mut Context<Self>) {
        if !self.signals.iter().any(|s| s.ticker.as_ref() == ticker) {
            return;
        }
        if self.selected.as_deref() == Some(ticker) {
            return;
        }
        self.selected = Some(SharedString::from(ticker.to_string()));
        cx.emit(SignalEvent::SelectionChanged);
        cx.notify();
    }

    /// Replace the entire signal list. Called by whichever component owns the
    /// engine integration (future): real implementations would diff the list
    /// and emit per-row updates, but for the mock list a full replace is fine.
    #[allow(dead_code)]
    pub fn set(&mut self, signals: Vec<Signal>, cx: &mut Context<Self>) {
        // Drop selection if the focused ticker disappeared.
        if let Some(sel) = &self.selected {
            if !signals.iter().any(|s| s.ticker == *sel) {
                self.selected = signals.first().map(|s| s.ticker.clone());
            }
        } else {
            self.selected = signals.first().map(|s| s.ticker.clone());
        }
        self.signals = signals;
        cx.emit(SignalEvent::Updated);
        cx.notify();
    }
}

#[derive(Clone)]
pub struct SignalServiceHandle(pub Entity<SignalService>);
impl Global for SignalServiceHandle {}

pub fn init(cx: &mut App) {
    let entity = cx.new(SignalService::new);
    cx.set_global(SignalServiceHandle(entity));
}

fn seed_signals() -> Vec<Signal> {
    use SignalDirection::*;
    let make = |ticker: &str,
                score: u8,
                setup: &str,
                reason: &str,
                direction: SignalDirection,
                tf: &str|
     -> Signal {
        Signal {
            ticker: ticker.into(),
            score,
            setup: setup.into(),
            reason: reason.into(),
            direction,
            timeframe: tf.into(),
        }
    };
    vec![
        make(
            "NVDA",
            92,
            "Breakout — continuation",
            "Holding above 50-day MA; relative strength vs. S&P at 6-mo high; volume 1.8× avg on yesterday's close.",
            Long,
            "Daily",
        ),
        make(
            "META",
            88,
            "Bull flag",
            "Tight 3-day consolidation after 8% impulse; tightening range on declining volume; pre-earnings drift in progress.",
            Long,
            "Daily",
        ),
        make(
            "AAPL",
            81,
            "Pullback to 20-EMA",
            "First pullback after breakout; held the 20-EMA on intraday flush; insider buys filed last week.",
            Long,
            "Daily",
        ),
        make(
            "TSLA",
            76,
            "Coiled spring",
            "Bollinger band squeeze at 6-month low width; price pinned to anchored VWAP from Q4 high.",
            Long,
            "Daily",
        ),
        make(
            "MSFT",
            71,
            "Range top retest",
            "Approaching prior multi-week resistance; needs volume confirmation. Watch for failure above 380.",
            Long,
            "Daily",
        ),
        make(
            "AMZN",
            65,
            "Mean reversion",
            "RSI(2) under 5 on daily; oversold into rising weekly trend. Bounce setup, tight stop required.",
            Long,
            "Daily",
        ),
        make(
            "GOOGL",
            58,
            "Channel midline",
            "Trading in well-defined ascending channel; mid-channel reaction zone is the entry trigger.",
            Long,
            "Daily",
        ),
        make(
            "BRK.B",
            54,
            "Quiet base",
            "Volatility crush + flat price for 6 weeks. Coil resolving above or below 410 sets direction.",
            Long,
            "Daily",
        ),
        make(
            "NFLX",
            48,
            "Lower-high formation",
            "Failed to reclaim broken support; relative weakness vs. peers expanding. Short bias on bounce.",
            Short,
            "Daily",
        ),
        make(
            "INTC",
            42,
            "Distribution",
            "5 distribution days in past 25 sessions; failing into supply on every rally attempt.",
            Short,
            "Daily",
        ),
    ]
}
