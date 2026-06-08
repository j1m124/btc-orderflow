//! Shared 4-section settings UI hosted by both the indicator-settings
//! floating window (VRVP) and the drawing-settings floating window (FRVP).
//!
//! Phase 1 stub. Real implementation lands in Phase 10. Both consumers
//! supply a `&mut VolumeProfileParams` plus a write-back closure invoked
//! whenever a control commits — the settings shell wraps that closure so
//! VRVP's persistence path and FRVP's `DrawingService::set_vp_params` write
//! through to disk without the shared view caring which is which.
