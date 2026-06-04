//! `BarStream`: the per-subscription ordering state machine for live bars.
//!
//! Owns the three concerns that the market_data service used to weave
//! together inline:
//! - **Pre-backfill buffering.** Ticks that arrive after the subscribe frame
//!   but before the REST backfill applies are held in `pending_ticks`.
//!   `apply_backfill` drains them through the same merge rule, so a bar that
//!   finalizes inside the protocol gap isn't lost.
//! - **Merge-or-append by `open_time`.** A tick whose `open_time` matches the
//!   last bar replaces it (in-progress update); a strictly newer one appends;
//!   anything older is dropped.
//! - **Sequence gap detection.** Each frame from the server carries a
//!   monotonic per-subscription `version` (`v`); a missing version anywhere
//!   in the sequence flips `Outcome::Gap`, which the caller turns into a
//!   re-snapshot (drop everything, spawn a fresh backfill).
//!
//! No async, no `Context`, no event emission. The caller drives state via
//! [`on_tick`] / [`apply_backfill`] / [`reset_for_backfill`] and reacts to
//! [`Outcome`]. That's the seam tests exploit.

use super::market_data::Candle;

/// State machine for one `(symbol, tf, session)` subscription's bar buffer.
pub(crate) struct BarStream {
    candles: Vec<Candle>,
    pending_ticks: Vec<(Candle, bool)>,
    backfill_done: bool,
    /// Last server-side sequence number processed. `None` means "no baseline
    /// yet" — the next tick is accepted as the new baseline regardless of its
    /// value (used both on fresh subscribe and after a gap-triggered reset).
    last_version: Option<u64>,
}

/// What the caller should do with the just-processed tick.
#[derive(Debug)]
pub(crate) enum Outcome {
    /// Tick was merged into `candles`. Caller should emit a `Tick` event.
    Applied { candle: Candle, is_closed: bool },
    /// Tick was buffered (pre-backfill) or merged-as-replace of last bar
    /// without producing a new entry. No event needs to be emitted.
    Buffered,
    /// Tick was dropped — either the pending buffer was full or the tick's
    /// `open_time` was older than the last bar. Caller may want to log.
    Dropped,
    /// Sequence gap: expected `last_seen + 1`, got `received`. Caller should
    /// reset and re-snapshot. One-shot — `last_version` is bumped to
    /// `received` so subsequent ticks don't re-trigger.
    Gap { last_seen: u64, received: u64 },
}

impl BarStream {
    pub(crate) fn new() -> Self {
        Self {
            candles: Vec::new(),
            pending_ticks: Vec::new(),
            backfill_done: false,
            last_version: None,
        }
    }

    pub(crate) fn candles(&self) -> &[Candle] {
        &self.candles
    }

    /// Mutable handle to the candle buffer — needed by the older-history page
    /// loader, which prepends pages and may trim from the front. Keep usage
    /// outside this module to history-paging only; live ticks must go through
    /// [`on_tick`].
    pub(crate) fn candles_mut(&mut self) -> &mut Vec<Candle> {
        &mut self.candles
    }

    /// Prepare for a fresh backfill: clears any in-flight pending ticks,
    /// flips `backfill_done` off, and forgets the version cursor so the next
    /// tick re-anchors. Called from `spawn_backfill` (new sub, reconnect, gap
    /// recovery).
    pub(crate) fn reset_for_backfill(&mut self) {
        self.backfill_done = false;
        self.pending_ticks.clear();
        self.last_version = None;
    }

    /// Apply a backfill response: replace `candles`, drain `pending_ticks`
    /// through the merge rule, mark backfill_done. The version cursor stays
    /// at `None` so the next live tick re-anchors the sequence.
    pub(crate) fn apply_backfill(&mut self, candles: Vec<Candle>) {
        self.candles = candles;
        self.backfill_done = true;
        self.last_version = None;
        let pending = std::mem::take(&mut self.pending_ticks);
        for (c, is_closed) in pending {
            let _ = self.merge_or_drop(c, is_closed);
        }
    }

    /// Process a live tick. Returns the action the caller should take.
    ///
    /// `version == 0` means "server didn't send a version field" — gap
    /// detection is skipped for this frame. The new server always sends
    /// `v >= 1` (counter incremented before first send), so v=0 reliably
    /// distinguishes an unversioned old server from a real sequence.
    pub(crate) fn on_tick(
        &mut self,
        candle: Candle,
        is_closed: bool,
        version: u64,
        pending_cap: usize,
    ) -> Outcome {
        if version > 0 {
            if let Some(last) = self.last_version {
                if version != last + 1 {
                    // One-shot gap signal: update the cursor so the *next* tick
                    // is treated as the new baseline (avoid re-firing Gap for
                    // every subsequent in-flight tick before the caller resets).
                    self.last_version = Some(version);
                    return Outcome::Gap {
                        last_seen: last,
                        received: version,
                    };
                }
            }
            self.last_version = Some(version);
        }

        if !self.backfill_done {
            if self.pending_ticks.len() >= pending_cap {
                return Outcome::Dropped;
            }
            self.pending_ticks.push((candle, is_closed));
            return Outcome::Buffered;
        }

        self.merge_or_drop(candle, is_closed)
    }

    /// Merge by `open_time`: equal → replace last (in-progress update);
    /// newer → push; older → drop.
    fn merge_or_drop(&mut self, candle: Candle, is_closed: bool) -> Outcome {
        let event_open = candle.open_time;
        match self.candles.last_mut() {
            Some(last) if last.open_time == event_open => {
                *last = candle.clone();
                Outcome::Applied { candle, is_closed }
            }
            Some(last) if last.open_time < event_open => {
                self.candles.push(candle.clone());
                Outcome::Applied { candle, is_closed }
            }
            None => {
                self.candles.push(candle.clone());
                Outcome::Applied { candle, is_closed }
            }
            Some(_) => Outcome::Dropped,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasm_bindgen_test::wasm_bindgen_test;

    fn c(open: i64, close: f64) -> Candle {
        Candle::new(open, open + 60_000, close, close, close, close, 0.0)
    }

    /// Post-backfill straight-through: each tick merges into candles in
    /// order, sequence numbers are accepted.
    #[wasm_bindgen_test]
    fn straight_through_post_backfill() {
        let mut s = BarStream::new();
        s.apply_backfill(vec![c(0, 100.0)]);
        // First post-backfill tick sets the baseline.
        let _ = s.on_tick(c(0, 101.0), false, 1, 200);
        let _ = s.on_tick(c(60_000, 102.0), true, 2, 200);
        let _ = s.on_tick(c(120_000, 103.0), false, 3, 200);
        assert_eq!(s.candles().len(), 3);
        assert_eq!(s.candles()[0].close, 101.0); // first bar updated in place
        assert_eq!(s.candles()[1].close, 102.0);
        assert_eq!(s.candles()[2].close, 103.0);
    }

    /// Market-open scenario: ticks arrive while backfill is in flight; one
    /// of them is a bar that finalized inside the protocol gap. Backfill
    /// applies, drain runs, and the finalized bar must be in candles.
    #[wasm_bindgen_test]
    fn market_open_buffered_drain() {
        let mut s = BarStream::new();
        // Backfill hasn't arrived yet — these all buffer.
        let r1 = s.on_tick(c(60_000, 100.0), true, 1, 200);
        let r2 = s.on_tick(c(120_000, 101.0), false, 2, 200);
        assert!(matches!(r1, Outcome::Buffered));
        assert!(matches!(r2, Outcome::Buffered));
        // Backfill arrives — note it doesn't yet contain the bar at 60_000
        // because the bar finalized DURING the REST round-trip.
        s.apply_backfill(vec![c(0, 99.0)]);
        // After drain, the finalized bar is in candles.
        assert_eq!(s.candles().len(), 3);
        assert_eq!(s.candles()[1].open_time, 60_000);
        assert_eq!(s.candles()[2].open_time, 120_000);
    }

    /// Sequence gap: client sees v1, v2, then v4 (v3 lost). Reports Gap
    /// exactly once; subsequent ticks proceed normally (caller is expected
    /// to call reset_for_backfill, but this test verifies one-shot).
    #[wasm_bindgen_test]
    fn gap_signal_is_one_shot() {
        let mut s = BarStream::new();
        s.apply_backfill(vec![c(0, 100.0)]);
        assert!(matches!(s.on_tick(c(0, 101.0), false, 1, 200), Outcome::Applied { .. }));
        assert!(matches!(s.on_tick(c(60_000, 102.0), false, 2, 200), Outcome::Applied { .. }));
        // v3 is missing.
        let gap = s.on_tick(c(120_000, 103.0), false, 4, 200);
        match gap {
            Outcome::Gap { last_seen, received } => {
                assert_eq!(last_seen, 2);
                assert_eq!(received, 4);
            }
            _ => panic!("expected Gap, got {:?}", gap),
        }
        // Following ticks do NOT re-fire Gap (caller hasn't reset yet, but
        // the one-shot semantics mean we don't keep yelling).
        let next = s.on_tick(c(180_000, 104.0), false, 5, 200);
        assert!(matches!(next, Outcome::Applied { .. }));
    }

    /// Out-of-order open_time within a single connection: should be dropped.
    #[wasm_bindgen_test]
    fn older_open_time_is_dropped() {
        let mut s = BarStream::new();
        s.apply_backfill(vec![c(60_000, 100.0)]);
        let _ = s.on_tick(c(120_000, 101.0), false, 1, 200);
        // A late tick from an earlier bar — drop.
        let r = s.on_tick(c(0, 99.0), false, 2, 200);
        assert!(matches!(r, Outcome::Dropped));
        assert_eq!(s.candles().len(), 2);
        assert_eq!(s.candles().last().unwrap().open_time, 120_000);
    }

    /// Pending buffer cap: once exceeded, return Dropped instead of growing
    /// unbounded.
    #[wasm_bindgen_test]
    fn pending_cap_is_enforced() {
        let mut s = BarStream::new();
        // Backfill_done is false (no apply_backfill called).
        for i in 0..200 {
            assert!(matches!(
                s.on_tick(c(60_000 * i, 100.0), false, (i + 1) as u64, 200),
                Outcome::Buffered
            ));
        }
        // 201st tick: cap reached.
        let r = s.on_tick(c(60_000 * 200, 100.0), false, 201, 200);
        assert!(matches!(r, Outcome::Dropped));
    }

    /// Unversioned (old-server) frames: every tick comes in with v=0 and
    /// the gap check is skipped. No false Gap events.
    #[wasm_bindgen_test]
    fn unversioned_frames_skip_gap_check() {
        let mut s = BarStream::new();
        s.apply_backfill(vec![c(0, 100.0)]);
        for i in 0..5 {
            let r = s.on_tick(c(60_000 * (i + 1), 100.0), false, 0, 200);
            assert!(matches!(r, Outcome::Applied { .. }));
        }
        assert_eq!(s.candles().len(), 6);
    }

    /// Re-snapshot path: gap → reset → fresh backfill → next tick re-anchors
    /// the version cursor without re-firing Gap.
    #[wasm_bindgen_test]
    fn gap_then_resnap_reanchors_cleanly() {
        let mut s = BarStream::new();
        s.apply_backfill(vec![c(0, 100.0)]);
        let _ = s.on_tick(c(0, 101.0), false, 1, 200);
        // Gap.
        let _ = s.on_tick(c(60_000, 102.0), false, 99, 200);
        // Caller responds: reset + new backfill arrives.
        s.reset_for_backfill();
        s.apply_backfill(vec![c(0, 200.0), c(60_000, 201.0)]);
        // Next live tick — any version is accepted as new baseline.
        let r = s.on_tick(c(120_000, 202.0), false, 873, 200);
        assert!(matches!(r, Outcome::Applied { .. }));
        // Following tick must be 874 — anything else would be a new gap.
        let r = s.on_tick(c(180_000, 203.0), false, 874, 200);
        assert!(matches!(r, Outcome::Applied { .. }));
    }
}
