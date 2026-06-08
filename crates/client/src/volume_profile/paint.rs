//! Shared rendering for VRVP overlay + FRVP drawing paint passes.
//!
//! Phase 1 stub — no signatures yet. The painter signature lands in Phase
//! 7 once we wire VRVP's overlay paint arm; FRVP joins in Phase 12.
//!
//! Planned signature (Phase 7):
//! ```ignore
//! pub fn paint_volume_profile(
//!     window: &mut Window,
//!     bounds: Bounds<Pixels>,
//!     anchor_x: Pixels,
//!     y_for_price: &dyn Fn(f64) -> Pixels,
//!     params: &VolumeProfileParams,
//!     output: &VolumeProfileOutput,
//! );
//! ```
