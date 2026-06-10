//! Build script that surfaces the build's commit SHA and ref name into the
//! compiled binary so the bottom bar can render e.g. `v0.1.0-abc1234`.
//!
//! `BUILD_SHA` and `BUILD_REF` are populated by the GHA workflow at image
//! build time. Outside CI they're unset — we emit empty strings so the
//! `env!()` invocations in `bottom_bar.rs` always succeed, and the bar
//! falls back to plain `v<CARGO_PKG_VERSION>` when both are empty.

fn main() {
    let sha = std::env::var("BUILD_SHA").unwrap_or_default();
    let reff = std::env::var("BUILD_REF").unwrap_or_default();
    println!("cargo:rustc-env=BUILD_SHA={sha}");
    println!("cargo:rustc-env=BUILD_REF={reff}");
    println!("cargo:rerun-if-env-changed=BUILD_SHA");
    println!("cargo:rerun-if-env-changed=BUILD_REF");
}
