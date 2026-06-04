//! In-memory orderbook + Binance diff-stream apply.
//!
//! The book is the single source of truth for the live orderbook panel and
//! the source for the 1s-cadence rows written to `book_snapshots`. It's
//! maintained by [`crate::ingest::run_book_maintainer`] against the
//! `@depth@100ms` diff stream, bootstrapped via a REST snapshot.
//!
//! Sequence semantics (Binance USD-M futures):
//!   - Each diff carries `first_update_id` (U), `final_update_id` (u), and
//!     `prev_final_update_id` (pu).
//!   - The first diff to apply after a REST snapshot has `last_update_id`
//!     L must satisfy `U <= L+1 <= u` (i.e. it overlaps or directly follows
//!     the snapshot's update id).
//!   - Subsequent diffs must satisfy `pu == previous.u` — any mismatch is
//!     a sequence gap and requires re-bootstrap.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use thiserror::Error;

use super::parse::DepthDiff;

/// `f64` price newtype that participates in `BTreeMap` ordering. Prices in
/// this codebase are never NaN — Binance returns finite decimal strings —
/// so we treat a NaN here as a programming error rather than a recoverable
/// data condition.
#[derive(Copy, Clone, Debug)]
pub struct Price(pub f64);

impl PartialEq for Price {
    fn eq(&self, other: &Self) -> bool {
        self.0.to_bits() == other.0.to_bits()
    }
}
impl Eq for Price {}

impl PartialOrd for Price {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Price {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0
            .partial_cmp(&other.0)
            .expect("orderbook price is NaN — Binance payload is malformed")
    }
}

#[derive(Debug, Error)]
pub enum BookSyncError {
    #[error("first diff out of range: U={u_first} u={u_final} last_snapshot_id={snapshot_id}")]
    FirstDiffOutOfRange {
        u_first: i64,
        u_final: i64,
        snapshot_id: i64,
    },
    #[error("sequence gap: pu={pu} != previous.u={prev_u}")]
    SequenceGap { pu: i64, prev_u: i64 },
}

/// Live orderbook state. `bids` is keyed by price; reverse-iteration walks
/// best (highest) bid first. `asks` is keyed by price; forward-iteration
/// walks best (lowest) ask first.
#[derive(Debug, Default, Clone)]
pub struct Book {
    pub bids: BTreeMap<Price, f64>,
    pub asks: BTreeMap<Price, f64>,
    /// `lastUpdateId` from the most recently applied event (or from the
    /// bootstrap REST snapshot before the first diff lands). -1 means
    /// "uninitialized".
    pub last_update_id: i64,
}

impl Book {
    pub fn empty() -> Self {
        Book {
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            last_update_id: -1,
        }
    }

    /// Replace state with a REST snapshot. Subsequent `apply_diff` calls
    /// must overlap or directly follow `last_update_id`.
    pub fn from_snapshot(
        bids: impl IntoIterator<Item = (f64, f64)>,
        asks: impl IntoIterator<Item = (f64, f64)>,
        last_update_id: i64,
    ) -> Self {
        let mut book = Book::empty();
        for (price, size) in bids {
            if size > 0.0 {
                book.bids.insert(Price(price), size);
            }
        }
        for (price, size) in asks {
            if size > 0.0 {
                book.asks.insert(Price(price), size);
            }
        }
        book.last_update_id = last_update_id;
        book
    }

    pub fn is_initialized(&self) -> bool {
        self.last_update_id >= 0
    }

    /// Apply a Binance diff event. Returns `Err` on a sequence violation —
    /// the maintainer should respond by re-bootstrapping from REST.
    pub fn apply_diff(&mut self, diff: &DepthDiff) -> Result<(), BookSyncError> {
        // First diff after a snapshot: U <= last_update_id+1 <= u.
        // After that: pu must equal the previously-applied u.
        if !self.is_initialized() {
            // No initialization at all — caller hasn't snapshotted yet. This
            // is a programmer error in our use; refuse to apply.
            return Err(BookSyncError::FirstDiffOutOfRange {
                u_first: diff.first_update_id,
                u_final: diff.final_update_id,
                snapshot_id: self.last_update_id,
            });
        }

        // After the snapshot, treat the first diff specially:
        if self.last_update_id != diff.prev_final_update_id {
            // Allow the first-diff overlap rule: U <= L+1 <= u.
            let l = self.last_update_id;
            let in_range =
                diff.first_update_id <= l + 1 && l + 1 <= diff.final_update_id;
            if !in_range {
                return Err(BookSyncError::SequenceGap {
                    pu: diff.prev_final_update_id,
                    prev_u: l,
                });
            }
        }

        for &(price, size) in &diff.bids {
            apply_level(&mut self.bids, price, size);
        }
        for &(price, size) in &diff.asks {
            apply_level(&mut self.asks, price, size);
        }

        self.last_update_id = diff.final_update_id;
        Ok(())
    }

    /// Top-N levels each side, best-first. Bids are descending by price;
    /// asks are ascending.
    pub fn top_n(&self, n: usize) -> (Vec<(f64, f64)>, Vec<(f64, f64)>) {
        let bids: Vec<(f64, f64)> = self
            .bids
            .iter()
            .rev()
            .take(n)
            .map(|(p, s)| (p.0, *s))
            .collect();
        let asks: Vec<(f64, f64)> = self
            .asks
            .iter()
            .take(n)
            .map(|(p, s)| (p.0, *s))
            .collect();
        (bids, asks)
    }
}

fn apply_level(side: &mut BTreeMap<Price, f64>, price: f64, size: f64) {
    if size > 0.0 {
        side.insert(Price(price), size);
    } else {
        side.remove(&Price(price));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diff(prev: i64, first: i64, last: i64, bids: &[(f64, f64)], asks: &[(f64, f64)]) -> DepthDiff {
        DepthDiff {
            symbol: "BTCUSDT".into(),
            event_time_ms: 0,
            first_update_id: first,
            final_update_id: last,
            prev_final_update_id: prev,
            bids: bids.to_vec(),
            asks: asks.to_vec(),
        }
    }

    #[test]
    fn applies_first_diff_overlapping_snapshot() {
        let mut book = Book::from_snapshot(
            vec![(100.0, 1.0), (99.0, 2.0)],
            vec![(101.0, 3.0)],
            10,
        );
        // U=8, u=11 → overlaps L+1=11 ✓
        let d = diff(7, 8, 11, &[(100.0, 1.5)], &[(101.0, 3.5)]);
        book.apply_diff(&d).expect("first diff applies");
        assert_eq!(book.last_update_id, 11);
        let (bids, asks) = book.top_n(5);
        assert_eq!(bids[0], (100.0, 1.5));
        assert_eq!(asks[0], (101.0, 3.5));
    }

    #[test]
    fn sequence_gap_on_pu_mismatch() {
        let mut book = Book::from_snapshot(vec![(100.0, 1.0)], vec![(101.0, 1.0)], 10);
        let d1 = diff(10, 11, 12, &[(100.5, 0.5)], &[]);
        book.apply_diff(&d1).unwrap();
        // Next diff claims previous.u was 13 — mismatch.
        let d2 = diff(13, 14, 15, &[], &[]);
        assert!(matches!(
            book.apply_diff(&d2),
            Err(BookSyncError::SequenceGap { .. })
        ));
    }

    #[test]
    fn size_zero_removes_level() {
        let mut book = Book::from_snapshot(vec![(100.0, 1.0)], vec![], 10);
        let d = diff(10, 11, 11, &[(100.0, 0.0)], &[]);
        book.apply_diff(&d).unwrap();
        assert!(book.bids.is_empty());
    }

    #[test]
    fn top_n_orders_best_first() {
        let book = Book::from_snapshot(
            vec![(100.0, 1.0), (99.0, 2.0), (101.0, 3.0)],
            vec![(105.0, 1.0), (103.0, 2.0), (107.0, 1.0)],
            0,
        );
        let (bids, asks) = book.top_n(3);
        assert_eq!(bids[0].0, 101.0); // highest bid first
        assert_eq!(bids[2].0, 99.0);
        assert_eq!(asks[0].0, 103.0); // lowest ask first
        assert_eq!(asks[2].0, 107.0);
    }
}
