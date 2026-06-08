//! Pure aggregation: footprint cells → per-price-bucket totals + Steidlmayer
//! value-area expansion. Stateless; the caller (VRVP indicator or FRVP
//! drawing paint) owns the cached output.
//!
//! Phase 1 stub — returns an empty output. Real implementation lands in
//! Phase 6 along with `#[cfg(test)]` coverage of POC selection, VA
//! expansion semantics, and one-sided / tied edge cases.

use super::output::VolumeProfileOutput;
use super::params::VolumeProfileParams;
use crate::services::market_data::FootprintCell;

/// Compute the per-bucket profile + reference levels for `cells` whose
/// `open_time` lands in `[range_ms.0, range_ms.1)`.
///
/// `range_ms.0` may equal `range_ms.1` (degenerate FRVP) — output is empty.
/// `cells` may be empty (subscription not yet warm) — output is empty with
/// `coverage_pct = 0.0`.
pub fn compute_volume_profile(
    _cells: &[FootprintCell],
    _range_ms: (i64, i64),
    _params: &VolumeProfileParams,
) -> VolumeProfileOutput {
    VolumeProfileOutput::default()
}
