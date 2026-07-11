//! Regenerate the checked-in spec (D69): writes
//! `crates/datboi-api/openapi.json` from the contract types and exits.
//! The staleness test (`checked_in_spec_is_current`) is what makes
//! forgetting to run this a red suite instead of a drift.

fn main() {
    // Compile-time manifest dir: this is a dev tool run from the
    // workspace checkout, not something that ships.
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/openapi.json");
    std::fs::write(path, datboi_api::openapi_json())
        .unwrap_or_else(|e| panic!("writing {path}: {e}"));
    println!("wrote {path}");
}
