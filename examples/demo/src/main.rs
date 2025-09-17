use chrono::{DateTime, Utc};
use reqwest::Url;
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Deserialize)]
struct ReleasePreview {
    name: String,
    published_at: DateTime<Utc>,
}

fn main() {
    // Treat require dependencies as if they were discovered via an API response.
    let payload = json!({
        "name": "cooldown-demo",
        "published_at": "2024-10-01T12:00:00Z"
    });

    let preview: ReleasePreview = serde_json::from_value(payload).expect("valid preview payload");
    let docs = Url::parse("https://doc.rust-lang.org/cargo/reference/specifying-dependencies.html")
        .expect("valid URL");

    println!(
        "crate `{}` was published at {}. Read more about dependency specs at {}",
        preview.name,
        preview.published_at,
        docs
    );
}
