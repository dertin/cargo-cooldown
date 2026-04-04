use std::process::Command;

use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use clap_cargo::Manifest;
use semver::{Version, VersionReq};

use crate::registry::{Release, ReleaseTimeline};

pub fn cutoff_time(minimum_minutes: u64, now: DateTime<Utc>) -> DateTime<Utc> {
    now - Duration::minutes(minimum_minutes as i64)
}

pub fn is_release_fresh(
    release: &Release,
    minimum_minutes: u64,
    now: DateTime<Utc>,
) -> Option<bool> {
    release
        .published_at
        .map(|published_at| published_at > cutoff_time(minimum_minutes, now))
}

pub fn select_candidate<'a>(
    timeline: &'a ReleaseTimeline,
    current_version: &str,
    requirements: &[VersionReq],
    minimum_minutes: u64,
    now: DateTime<Utc>,
) -> Option<&'a Release> {
    let cutoff = cutoff_time(minimum_minutes, now);
    let current = Version::parse(current_version).ok()?;

    timeline
        .releases
        .iter()
        .rev()
        .filter(|release| !release.yanked)
        .filter_map(|release| {
            let published_at = release.published_at?;
            if published_at > cutoff {
                return None;
            }

            let parsed = Version::parse(&release.version).ok()?;
            if parsed >= current {
                return None;
            }
            if requirements
                .iter()
                .all(|requirement| requirement.matches(&parsed))
            {
                Some(release)
            } else {
                None
            }
        })
        .next()
}

#[derive(Debug)]
pub enum PinOutcome {
    Applied,
    Rejected { stdout: String, stderr: String },
}

pub fn try_pin_precise(
    manifest: &Manifest,
    name: &str,
    current: &str,
    version: &str,
) -> Result<PinOutcome> {
    let spec = format!("{name}@{current}");
    let mut command = Command::new("cargo");
    command.arg("update");
    if let Some(path) = &manifest.manifest_path {
        command.arg("--manifest-path").arg(path);
    }
    let output = command.args(["-p", &spec, "--precise", version]).output()?;
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
    use semver::VersionReq;

    use crate::registry::ReleaseSource;

    fn timeline() -> ReleaseTimeline {
        ReleaseTimeline {
            releases: vec![
                Release {
                    version: "1.0.0".into(),
                    published_at: Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()),
                    yanked: false,
                    source: ReleaseSource::Index,
                },
                Release {
                    version: "1.1.0".into(),
                    published_at: Some(Utc.with_ymd_and_hms(2026, 2, 1, 0, 0, 0).unwrap()),
                    yanked: false,
                    source: ReleaseSource::Index,
                },
                Release {
                    version: "1.2.0".into(),
                    published_at: Some(Utc.with_ymd_and_hms(2026, 3, 28, 0, 0, 0).unwrap()),
                    yanked: false,
                    source: ReleaseSource::Index,
                },
            ],
        }
    }

    #[test]
    fn selects_newest_compatible_release_before_cutoff() {
        let now = Utc.with_ymd_and_hms(2026, 4, 3, 0, 0, 0).unwrap();
        let requirements = vec![VersionReq::parse("^1").unwrap()];
        let timeline = timeline();
        let candidate = select_candidate(&timeline, "1.2.0", &requirements, 14 * 24 * 60, now)
            .expect("candidate should exist");
        assert_eq!(candidate.version, "1.1.0");
    }

    #[test]
    fn ignores_yanked_or_missing_timestamps() {
        let now = Utc.with_ymd_and_hms(2026, 4, 3, 0, 0, 0).unwrap();
        let requirements = vec![VersionReq::parse("^1").unwrap()];
        let mut timeline = timeline();
        timeline.releases[1].yanked = true;
        timeline.releases[0].published_at = None;

        assert!(select_candidate(&timeline, "1.2.0", &requirements, 14 * 24 * 60, now).is_none());
    }

    #[test]
    fn reports_freshness_when_timestamp_is_available() {
        let now = Utc.with_ymd_and_hms(2026, 4, 3, 0, 0, 0).unwrap();
        assert_eq!(
            is_release_fresh(&timeline().releases[2], 14 * 24 * 60, now,),
            Some(true)
        );
    }
}
