//! Small crates.io-backed workspace used by smoke tests and benchmarks.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Deserialize)]
struct ReleasePreview {
    name: String,
    released: DateTime<Utc>,
}

fn main() {
    // Treat require dependencies as if they were discovered via an API response.
    let payload = json!({
        "name": "cargo-cooldown",
        "released": Utc::now(),
    });
    let preview: ReleasePreview = serde_json::from_value(payload).expect("valid preview payload");
    let docs = "https://doc.rust-lang.org/cargo/reference/specifying-dependencies.html";

    println!(
        "Example: `{}` was released at {}, see {} for details.",
        preview.name, preview.released, docs
    );
}
