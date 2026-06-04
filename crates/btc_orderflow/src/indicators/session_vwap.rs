//! Session VWAP overlay. Cumulative volume-weighted average price anchored
//! at 09:30 ET each weekday; resets on the next session open.
//!
//! Math: for bars at/after 09:30 ET on the bar's ET trading date,
//! `vwap_i = Σ(bar_vwap_k · bar_volume_k) / Σ(bar_volume_k)` over k in the
//! session through bar i. Bars before 09:30 ET (pre-market on ETH charts)
//! emit `None`, so the line only renders from the regular open onward.
//!
//! `Candle.vwap` comes from the server (per-bar VWAP). Bars missing VWAP
//! contribute nothing to the accumulator. Hidden on 1d: paint-side guard in
//! `chart.rs` skips this `kind_id` when the chart timeframe is daily.

use chrono::{Datelike, Timelike, Weekday};
use chrono_tz::US::Eastern;
use gpui::SharedString;
use serde::{Deserialize, Serialize};

use super::kind::{IndicatorKind, PaneKind};
use super::output::{IndicatorOutput, Series, ValueReadout};
use crate::services::market_data::Candle;

/// Session VWAP carries no tunable params today — anchor is fixed at the
/// regular session open. Kept as a struct so future knobs (anchor time,
/// extended-hours inclusion) slot in without changing the registration shape.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionVwapParams {}

const RTH_OPEN_MIN: u32 = 9 * 60 + 30;

/// (year, ordinal-day) in ET, used as a stable session key. Yearly rollover is
/// rare enough that we don't bother packing month/day.
fn et_session_key(ms: i64) -> Option<(i32, u32, bool)> {
    use chrono::TimeZone as _;
    let et = Eastern.timestamp_millis_opt(ms).single()?;
    let weekday = !matches!(et.weekday(), Weekday::Sat | Weekday::Sun);
    Some((et.year(), et.ordinal(), weekday))
}

fn et_minute_of_day(ms: i64) -> Option<u32> {
    use chrono::TimeZone as _;
    let et = Eastern.timestamp_millis_opt(ms).single()?;
    Some(et.hour() * 60 + et.minute())
}

impl IndicatorKind for SessionVwapParams {
    fn kind_id(&self) -> &'static str {
        "session_vwap"
    }
    fn pane_kind(&self) -> PaneKind {
        PaneKind::OverlayOnly
    }
    fn label(&self) -> SharedString {
        "VWAP".into()
    }
    fn compute(&self, candles: &[Candle]) -> IndicatorOutput {
        let n = candles.len();
        let mut out: Series = vec![None; n];
        let mut cur_session: Option<(i32, u32)> = None;
        let mut num = 0.0_f64;
        let mut den = 0.0_f64;
        for (i, c) in candles.iter().enumerate() {
            let Some((y, doy, weekday)) = et_session_key(c.open_time) else {
                continue;
            };
            // Session rollover: any change in ET date resets accumulators.
            if cur_session != Some((y, doy)) {
                cur_session = Some((y, doy));
                num = 0.0;
                den = 0.0;
            }
            if !weekday {
                continue;
            }
            let Some(min) = et_minute_of_day(c.open_time) else {
                continue;
            };
            if min < RTH_OPEN_MIN {
                continue;
            }
            if let Some(vw) = c.vwap {
                if vw > 0.0 && c.volume > 0.0 {
                    num += vw * c.volume;
                    den += c.volume;
                }
            }
            if den > 0.0 {
                out[i] = Some(num / den);
            }
        }
        IndicatorOutput::Line(out)
    }
    fn value_at(&self, output: &IndicatorOutput, index: usize) -> ValueReadout {
        match output {
            IndicatorOutput::Line(s) => ValueReadout::One(s.get(index).copied().flatten()),
            _ => ValueReadout::One(None),
        }
    }
    fn y_range(&self, output: &IndicatorOutput, range: std::ops::Range<usize>) -> Option<(f64, f64)> {
        let IndicatorOutput::Line(s) = output else {
            return None;
        };
        let lo_i = range.start.min(s.len());
        let hi_i = range.end.min(s.len());
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        let mut any = false;
        for v in &s[lo_i..hi_i] {
            if let Some(v) = v {
                if *v < min {
                    min = *v;
                }
                if *v > max {
                    max = *v;
                }
                any = true;
            }
        }
        any.then_some((min, max))
    }
    fn params_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasm_bindgen_test::wasm_bindgen_test;

    fn candle(open_time: i64, vwap: f64, vol: f64) -> Candle {
        Candle::new_full(
            open_time,
            open_time + 60_000,
            0.0,
            0.0,
            0.0,
            0.0,
            vol,
            Some(vwap),
            None,
        )
    }

    /// 09:30 ET = 13:30 UTC during EDT (2024-05-24 is in EDT, UTC-4). Use a
    /// known weekday so the weekday filter doesn't reject the test fixture.
    fn et_open_ms_2024_05_24_edt() -> i64 {
        // 2024-05-24 13:30 UTC == 2024-05-24 09:30 EDT.
        1716557400000
    }

    #[wasm_bindgen_test]
    fn vwap_accumulates_from_session_open() {
        // Three RTH bars within a single session, weighted by volume.
        let open = et_open_ms_2024_05_24_edt();
        let bars = vec![
            candle(open, 10.0, 100.0),
            candle(open + 60_000, 20.0, 200.0),
            candle(open + 120_000, 30.0, 300.0),
        ];
        let out = SessionVwapParams::default().compute(&bars);
        let IndicatorOutput::Line(s) = out else {
            panic!("expected Line output");
        };
        assert_eq!(s[0], Some(10.0));
        // Σ(10*100 + 20*200) / 300 = 5000/300
        let want_1 = (10.0 * 100.0 + 20.0 * 200.0) / 300.0;
        assert!((s[1].unwrap() - want_1).abs() < 1e-9);
        // Σ(10*100 + 20*200 + 30*300) / 600
        let want_2 = (10.0 * 100.0 + 20.0 * 200.0 + 30.0 * 300.0) / 600.0;
        assert!((s[2].unwrap() - want_2).abs() < 1e-9);
    }

    #[wasm_bindgen_test]
    fn premarket_bars_emit_none() {
        let open = et_open_ms_2024_05_24_edt();
        // Bar 1 minute before 09:30 ET, with a real VWAP — should still be None.
        let pre = candle(open - 60_000, 9.5, 50.0);
        let rth = candle(open, 10.0, 100.0);
        let out = SessionVwapParams::default().compute(&[pre, rth]);
        let IndicatorOutput::Line(s) = out else {
            panic!("expected Line output");
        };
        assert_eq!(s[0], None);
        assert_eq!(s[1], Some(10.0));
    }

    #[wasm_bindgen_test]
    fn new_session_resets_accumulator() {
        let day1 = et_open_ms_2024_05_24_edt();
        // 2024-05-28 09:30 EDT (next trading day after Memorial Day weekend).
        let day2 = 1716903000000;
        let bars = vec![
            candle(day1, 100.0, 1000.0),
            candle(day2, 200.0, 500.0),
        ];
        let out = SessionVwapParams::default().compute(&bars);
        let IndicatorOutput::Line(s) = out else {
            panic!("expected Line output");
        };
        assert_eq!(s[0], Some(100.0));
        // Second day must NOT carry day-1's accumulator forward.
        assert_eq!(s[1], Some(200.0));
    }

    #[wasm_bindgen_test]
    fn missing_vwap_contributes_nothing() {
        let open = et_open_ms_2024_05_24_edt();
        let with_vw = candle(open, 10.0, 100.0);
        // Volume present but vwap is None (developing bar w/o trade-count fix).
        let no_vw = Candle::new_full(open + 60_000, open + 120_000, 0.0, 0.0, 0.0, 0.0, 200.0, None, None);
        let out = SessionVwapParams::default().compute(&[with_vw, no_vw]);
        let IndicatorOutput::Line(s) = out else {
            panic!("expected Line output");
        };
        // Both bars share the same running figure because the second bar
        // didn't contribute to num/den.
        assert_eq!(s[0], Some(10.0));
        assert_eq!(s[1], Some(10.0));
    }
}
