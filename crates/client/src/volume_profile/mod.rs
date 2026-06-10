//! Shared volume-profile machinery — backs both the **VRVP** indicator
//! (`crate::indicators::vrvp`) and the **FRVP** drawing tool
//! (`crate::drawings::shapes::DrawingShape::Frvp`). Both compute the same
//! per-price-bucket aggregation off `FootprintCell` data; they only differ
//! in which time range they cover (VRVP = chart's visible bar window; FRVP
//! = user-anchored fixed range).
//!
//! Module layout:
//! - [`params`]: serde params struct + enum knobs (mode / scaling / anchor).
//! - [`output`]: per-bucket data + POC/VAH/VAL plumbed back to the painter.
//! - [`compute`]: pure aggregation + Steidlmayer value-area expansion.
//! - [`paint`]: bar + reference-line rendering, shared by overlay paint
//!   (VRVP) and drawing paint (FRVP).
//!
//! The VP settings UI (form fields for buckets / levels / colors) is
//! declared inline on the VRVP indicator and the FRVP drawing kind via
//! the shared `crate::settings_form` framework — no per-VP shell module.

pub mod compute;
pub mod output;
pub mod paint;
pub mod params;

pub use compute::compute_volume_profile;
pub use output::{VolumeProfileOutput, VpBucket};
pub use params::{AnchorEdge, VolumeProfileParams, VpDeltaScale, VpRenderMode};
