//! Output emitted by `IndicatorKind::compute`. Variants are matched on by
//! the paint pipeline to pick the right draw primitive (line, histogram,
//! filled band, multi-series MACD shape). All variants store series of
//! length `candles.len()` so the chart can index directly by bar position.

/// Per-bar series. `None` is emitted for leading bars before the indicator
/// has enough history — the paint pass skips those positions instead of
/// drawing through a sentinel like NaN.
pub type Series = Vec<Option<f64>>;

/// Concrete shapes any v1 indicator can emit. The paint pipeline matches
/// on this to draw the right primitive; the value-readout pipeline matches
/// on it to format the crosshair chip readout.
#[derive(Clone, Debug)]
pub enum IndicatorOutput {
    /// Single line. Used by RSI; standalone lines.
    Line(Series),

    /// Arbitrary-count parallel line series — one per "slot" the kind
    /// declares. Drives multi-line kinds where the count varies per
    /// instance (the MA Suite holds N user-configured EMA/SMA entries
    /// and emits N parallel lines). Paint colors each series with the
    /// matching `colors[i]` slot.
    Lines(Vec<Series>),

    /// Per-bar histogram (Volume). `up[i] = true` colors bar `i` with the
    /// up-color; `false` uses the down-color. Polarity is decided by the
    /// caller (e.g., `bar.close >= bar.open`).
    Histogram { values: Series, up: Vec<bool> },

    /// Three-line envelope (Bollinger Bands). Paint draws the middle line
    /// and the upper/lower envelopes, optionally filling the area between
    /// upper and lower with low-alpha fill.
    Bands {
        upper: Series,
        middle: Series,
        lower: Series,
    },

    /// MACD shape: MACD line, signal line, and histogram (MACD - signal).
    /// Paint draws the histogram bars first, then the two lines on top.
    Macd {
        macd: Series,
        signal: Series,
        histogram: Series,
    },

    /// Per-bar statistic row: total volume + signed delta stacked as two
    /// text rows inside one cell per bar. Volume cell paints with a fixed
    /// blue base tint; delta cell paints bull/bear based on the sign of
    /// the delta itself. `daily_max_vol`/`daily_max_delta` are per-bar
    /// rolling 24h maxima of the same series (precomputed so the paint
    /// pass doesn't recompute every frame); paint divides into them to
    /// derive the "Daily" gradient intensity. `grade` is mirrored into
    /// the output so the paint pass picks up grading-mode changes via the
    /// normal `update_indicator` → `recompute_indicators` flow without
    /// PanePaintItem needing a new field.
    BarStat {
        grade: crate::indicators::BarStatGrade,
        volume: Series,
        delta: Series,
        daily_max_vol: Series,
        daily_max_delta: Series,
    },
}

impl IndicatorOutput {
    pub fn len(&self) -> usize {
        match self {
            IndicatorOutput::Line(s) => s.len(),
            IndicatorOutput::Lines(ss) => ss.first().map(|s| s.len()).unwrap_or(0),
            IndicatorOutput::Histogram { values, .. } => values.len(),
            IndicatorOutput::Bands { upper, .. } => upper.len(),
            IndicatorOutput::Macd { macd, .. } => macd.len(),
            IndicatorOutput::BarStat { volume, .. } => volume.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Chip-readout shape returned by `IndicatorKind::value_at`. The chip view
/// formats this to "Label: v1" / "Label: v1 / v2" / etc. depending on the
/// variant. `None` slots render as "—" so a no-history bar still produces
/// readable copy.
#[derive(Clone, Debug)]
pub enum ValueReadout {
    /// Single scalar (RSI, Volume).
    One(Option<f64>),
    /// Two scalars (BB → upper / lower; middle is implied by the label).
    Two(Option<f64>, Option<f64>),
    /// Three scalars (MACD → macd / signal / histogram).
    Three(Option<f64>, Option<f64>, Option<f64>),
    /// Arbitrary-count readout, one entry per parallel series (the MA
    /// Suite returns one value per user-configured MA). Formatted as
    /// "v1 / v2 / ..." in the chip; empty Vec collapses to "—".
    Many(Vec<Option<f64>>),
}
