//! Candidate selection over release timelines and cooldown cutoffs.
//!
//! This module does the small, pure decision of choosing which release versions
//! are eligible. It does not touch Cargo or the lockfile; callers provide a
//! timeline, semver requirements, current time, and baseline policy callback.

use chrono::{DateTime, Duration, Utc};
use semver::{Version, VersionReq};

use crate::registry::{Release, ReleaseTimeline};

/// Return the oldest publish time that is still accepted by the cooldown window.
pub fn cutoff_time(minimum_minutes: u64, now: DateTime<Utc>) -> DateTime<Utc> {
    now - Duration::minutes(minimum_minutes as i64)
}

/// Determine whether a release is fresh when its publish time is known.
pub fn is_release_fresh(
    release: &Release,
    minimum_minutes: u64,
    now: DateTime<Utc>,
) -> Option<bool> {
    release
        .published_at
        .map(|published_at| published_at > cutoff_time(minimum_minutes, now))
}

/// Select the newest older compatible release accepted by cooldown.
///
/// The caller supplies the release timeline, the currently locked version,
/// semver requirements that must remain true, the cooldown window, and a
/// baseline callback for versions already allowed by the initial lockfile. The
/// returned release is older than `current_version`, not yanked, requirement
/// compatible, and either old enough or explicitly allowed by the baseline.
pub fn select_candidate<'a>(
    timeline: &'a ReleaseTimeline,
    current_version: &str,
    requirements: &[VersionReq],
    minimum_minutes: u64,
    now: DateTime<Utc>,
    baseline_allows: impl Fn(&str) -> bool,
) -> Option<&'a Release> {
    select_candidates(
        timeline,
        current_version,
        requirements,
        minimum_minutes,
        now,
        baseline_allows,
        1,
    )
    .into_iter()
    .next()
}

/// Select up to `limit` older compatible releases, newest first.
///
/// This is the broader form used by local and coordinated solvers. It walks the
/// timeline from newest to oldest, filters out yanked, fresh, too-new, or
/// requirement-incompatible releases, and returns candidates in the order the
/// solver should try them.
pub fn select_candidates<'a, F>(
    timeline: &'a ReleaseTimeline,
    current_version: &str,
    requirements: &[VersionReq],
    minimum_minutes: u64,
    now: DateTime<Utc>,
    baseline_allows: F,
    limit: usize,
) -> Vec<&'a Release>
where
    F: Fn(&str) -> bool,
{
    let cutoff = cutoff_time(minimum_minutes, now);
    let Some(current) = Version::parse(current_version).ok() else {
        return Vec::new();
    };

    let mut candidates = Vec::new();
    for release in timeline
        .releases
        .iter()
        .rev()
        .filter(|release| !release.yanked)
    {
        let baseline_allowed = baseline_allows(&release.version);
        if !baseline_allowed {
            let Some(published_at) = release.published_at else {
                continue;
            };
            if published_at > cutoff {
                continue;
            }
        }

        let Some(parsed) = Version::parse(&release.version).ok() else {
            continue;
        };
        if parsed >= current {
            continue;
        }
        if requirements
            .iter()
            .all(|requirement| requirement.matches(&parsed))
        {
            candidates.push(release);
            if candidates.len() >= limit {
                break;
            }
        }
    }

    candidates
}

/// Unit tests for cutoff and candidate selection behavior.
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
        let candidate =
            select_candidate(&timeline, "1.2.0", &requirements, 14 * 24 * 60, now, |_| {
                false
            })
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

        assert!(
            select_candidate(&timeline, "1.2.0", &requirements, 14 * 24 * 60, now, |_| {
                false
            })
            .is_none()
        );
    }

    #[test]
    fn allows_baseline_versions_even_when_they_are_still_fresh() {
        let now = Utc.with_ymd_and_hms(2026, 4, 3, 0, 0, 0).unwrap();
        let requirements = vec![VersionReq::parse("^1").unwrap()];
        let timeline = ReleaseTimeline {
            releases: vec![
                Release {
                    version: "1.0.0".into(),
                    published_at: Some(Utc.with_ymd_and_hms(2026, 3, 31, 0, 0, 0).unwrap()),
                    yanked: false,
                    source: ReleaseSource::Index,
                },
                Release {
                    version: "1.1.0".into(),
                    published_at: Some(Utc.with_ymd_and_hms(2026, 4, 2, 0, 0, 0).unwrap()),
                    yanked: false,
                    source: ReleaseSource::Index,
                },
            ],
        };

        let candidate = select_candidate(&timeline, "1.1.0", &requirements, 14 * 24 * 60, now, {
            |version| version == "1.0.0"
        })
        .expect("baseline version should remain eligible");

        assert_eq!(candidate.version, "1.0.0");
    }

    #[test]
    fn select_candidates_returns_multiple_options_in_descending_order() {
        let now = Utc.with_ymd_and_hms(2026, 4, 3, 0, 0, 0).unwrap();
        let requirements = vec![VersionReq::parse("^1").unwrap()];
        let timeline = timeline();
        let candidates = select_candidates(
            &timeline,
            "1.2.0",
            &requirements,
            14 * 24 * 60,
            now,
            |_| false,
            2,
        );

        assert_eq!(
            candidates
                .iter()
                .map(|release| release.version.as_str())
                .collect::<Vec<_>>(),
            vec!["1.1.0", "1.0.0"]
        );
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
