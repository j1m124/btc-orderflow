//! MA Suite — a single overlay indicator holding a user-managed list of
//! moving averages. Each entry picks SMA or EMA, its period, and its
//! price source. Replaces the separate SMA + EMA picker entries: one
//! suite chip in the toolbar, one settings form with add/remove rows,
//! and a single recompute pass over the candles for the full set.
//!
//! Color is carried per-entry via the kind's `color_slots()` — the slot
//! count matches `entries.len()`, so each MA gets its own slot in
//! `IndicatorInstance.colors` and its own row in the settings color
//! picker section.

use gpui::SharedString;
use serde::{Deserialize, Serialize};

use super::kind::{IndicatorKind, PaneKind, Source};
use super::math::{extract_source, line_range, rolling_ema, rolling_sma};
use super::output::{IndicatorOutput, Series, ValueReadout};
use crate::services::market_data::Candle;

/// Which moving-average flavor a single entry computes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MaFlavor {
    Sma,
    Ema,
}

impl MaFlavor {
    /// Short user-facing tag — embedded in slot labels and the chip
    /// label so the user can tell `EMA 20` apart from `SMA 50` at a
    /// glance.
    pub fn tag(self) -> &'static str {
        match self {
            MaFlavor::Sma => "SMA",
            MaFlavor::Ema => "EMA",
        }
    }
}

/// One row inside an MA Suite: a single flavor + period + source.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MaEntry {
    pub flavor: MaFlavor,
    pub period: usize,
    pub source: Source,
}

impl MaEntry {
    /// Default new entry — EMA 20 on close. Common starting point that
    /// matches what the picker used to add when "EMA" was its own kind.
    pub fn default_new() -> Self {
        Self {
            flavor: MaFlavor::Ema,
            period: 20,
            source: Source::Close,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MaSuiteParams {
    pub entries: Vec<MaEntry>,
}

impl Default for MaSuiteParams {
    fn default() -> Self {
        // One default entry so a freshly-added suite is visible
        // immediately rather than rendering as an empty chip.
        Self {
            entries: vec![MaEntry::default_new()],
        }
    }
}

impl IndicatorKind for MaSuiteParams {
    fn kind_id(&self) -> &'static str {
        "ma_suite"
    }
    fn pane_kind(&self) -> PaneKind {
        PaneKind::OverlayOnly
    }
    fn label(&self) -> SharedString {
        if self.entries.is_empty() {
            return SharedString::from("MA");
        }
        // Compact summary: "MA: 20EMA, 50SMA". Cap at 3 entries so a
        // long suite doesn't blow out the chip width — chips after that
        // are summarized as "+N".
        const MAX_INLINE: usize = 3;
        let shown: Vec<String> = self
            .entries
            .iter()
            .take(MAX_INLINE)
            .map(|e| format!("{}{}", e.period, e.flavor.tag()))
            .collect();
        let suffix = if self.entries.len() > MAX_INLINE {
            format!(" +{}", self.entries.len() - MAX_INLINE)
        } else {
            String::new()
        };
        SharedString::from(format!("MA: {}{}", shown.join(", "), suffix))
    }
    fn compute(&self, candles: &[Candle]) -> IndicatorOutput {
        let mut series: Vec<Series> = Vec::with_capacity(self.entries.len());
        for e in &self.entries {
            let src = extract_source(candles, e.source);
            let s = match e.flavor {
                MaFlavor::Sma => rolling_sma(&src, e.period),
                MaFlavor::Ema => rolling_ema(&src, e.period),
            };
            series.push(s);
        }
        IndicatorOutput::Lines(series)
    }
    fn value_at(&self, output: &IndicatorOutput, index: usize) -> ValueReadout {
        match output {
            IndicatorOutput::Lines(ss) => ValueReadout::Many(
                ss.iter().map(|s| s.get(index).copied().flatten()).collect(),
            ),
            _ => ValueReadout::Many(Vec::new()),
        }
    }
    fn y_range(
        &self,
        output: &IndicatorOutput,
        range: std::ops::Range<usize>,
    ) -> Option<(f64, f64)> {
        let IndicatorOutput::Lines(ss) = output else {
            return None;
        };
        let mut combined: Option<(f64, f64)> = None;
        for s in ss {
            let Some((lo, hi)) = line_range(s, range.clone()) else {
                continue;
            };
            combined = Some(match combined {
                None => (lo, hi),
                Some((a, b)) => (a.min(lo), b.max(hi)),
            });
        }
        combined
    }
    fn params_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn color_slots(&self) -> Vec<SharedString> {
        // One slot per entry, labeled with the entry's flavor + period
        // so the settings color picker section reads as "EMA 20",
        // "SMA 50", … and the user can map each picker swatch back to
        // a specific MA at a glance.
        self.entries
            .iter()
            .map(|e| SharedString::from(format!("{} {}", e.flavor.tag(), e.period)))
            .collect()
    }
}
