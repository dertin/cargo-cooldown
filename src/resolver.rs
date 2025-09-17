use std::process::Command;

use anyhow::Result;
use chrono::{DateTime, Duration, Utc};

use crate::registry::VersionMeta;

#[derive(Debug, Clone)]
pub struct Candidate {
    pub version: String,
    pub created_at: DateTime<Utc>,
}

pub fn filter_candidates(
    versions: Vec<VersionMeta>,
    minimum_minutes: u64,
    now: DateTime<Utc>,
) -> Vec<Candidate> {
    let cutoff = now - Duration::minutes(minimum_minutes as i64);
    let mut filtered: Vec<Candidate> = versions
        .into_iter()
        .filter(|meta| !meta.yanked)
        .filter(|meta| meta.created_at <= cutoff)
        .map(|meta| Candidate {
            version: meta.num,
            created_at: meta.created_at,
        })
        .collect();
    filtered.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    filtered
}

#[derive(Debug)]
pub enum PinOutcome {
    Applied,
    Rejected { stdout: String, stderr: String },
}

pub fn try_pin_precise(name: &str, current: &str, version: &str) -> Result<PinOutcome> {
    let spec = format!("{name}@{current}");
    let output = Command::new("cargo")
        .args(["update", "-p", &spec, "--precise", version])
        .output()?;
    if output.status.success() {
        Ok(PinOutcome::Applied)
    } else {
        Ok(PinOutcome::Rejected {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn filters_fresh_versions() {
        let now = Utc.with_ymd_and_hms(2024, 10, 1, 0, 0, 0).unwrap();
        let versions = vec![
            VersionMeta {
                created_at: Utc.with_ymd_and_hms(2024, 9, 30, 23, 50, 0).unwrap(),
                yanked: false,
                num: "1.2.3".into(),
            },
            VersionMeta {
                created_at: Utc.with_ymd_and_hms(2024, 9, 30, 22, 0, 0).unwrap(),
                yanked: false,
                num: "1.2.2".into(),
            },
            VersionMeta {
                created_at: Utc.with_ymd_and_hms(2024, 9, 30, 20, 0, 0).unwrap(),
                yanked: true,
                num: "1.2.1".into(),
            },
        ];
        let candidates = filter_candidates(versions, 30, now);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].version, "1.2.2");
    }
}
