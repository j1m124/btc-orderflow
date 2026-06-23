//! Build script that surfaces the build's commit SHA + ref name into the
//! compiled server binary so `/healthz` can report which build is live.
//!
//! Mirrors `crates/client/build.rs`. With server and client now deployed on
//! separate cadences, the bottom-bar version string reports the *client*
//! build; this lets `curl /healthz` confirm the *server* build independently
//! (see CLAUDE.md "Split deployment"). `BUILD_SHA`/`BUILD_REF` are populated
//! by the GHA `server.yml` workflow at image build time; outside CI they're
//! unset and we emit empty strings so the `env!()` lookups always succeed.

fn main() {
    let sha = std::env::var("BUILD_SHA").unwrap_or_default();
    let reff = std::env::var("BUILD_REF").unwrap_or_default();
    println!("cargo:rustc-env=BUILD_SHA={sha}");
    println!("cargo:rustc-env=BUILD_REF={reff}");
    println!("cargo:rerun-if-env-changed=BUILD_SHA");
    println!("cargo:rerun-if-env-changed=BUILD_REF");
}
