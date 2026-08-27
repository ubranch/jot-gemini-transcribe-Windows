// Copyright 2026 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! "Is there a newer Jot?"
//!
//! Deliberately not a self-updater, and deliberately not on a timer. Jot's
//! promise is that nothing leaves this PC unattended except the audio you
//! dictate, so this asks GitHub exactly one question, exactly when you open
//! the About window, and then tells you where to click. Downloading and
//! swapping the executable is your decision, not the app's.

use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};

/// The web page, not `api.github.com`. The JSON API allows 60 unauthenticated
/// requests per hour *per IP*, and behind carrier-grade NAT — normal for much
/// of the world — a user can be rate-limited by strangers before Jot asks
/// anything. This URL answers 302 to the newest tag with no such limit, and
/// the tag is the only fact needed.
const LATEST_RELEASE: &str =
    "https://github.com/ubranch/jot-gemini-transcribe-Windows/releases/latest";

/// GitHub answers slowly or not at all more often than it answers wrongly, and
/// a stalled About window is worse than an unanswered question.
const DEADLINE: Duration = Duration::from_secs(10);

/// A release newer than the one running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    pub version: String,
    pub page: String,
}

/// Asks GitHub for the newest release. `Ok(None)` means this build is current.
pub async fn check(current: &str) -> Result<Option<Update>> {
    let response = reqwest::Client::builder()
        // The redirect *is* the answer, so following it would throw it away and
        // download a page of HTML for nothing.
        .redirect(reqwest::redirect::Policy::none())
        .timeout(DEADLINE)
        .build()
        .context("building the update-check client")?
        .head(LATEST_RELEASE)
        .header(
            reqwest::header::USER_AGENT,
            concat!("jot/", env!("CARGO_PKG_VERSION")),
        )
        .send()
        .await
        .context("asking GitHub for the latest release")?;

    let page = response
        .headers()
        .get(reqwest::header::LOCATION)
        .ok_or_else(|| anyhow!("GitHub answered {} without a redirect", response.status()))?
        .to_str()
        .context("GitHub sent a redirect that is not text")?
        .to_string();

    let version = release_tag(&page)
        .ok_or_else(|| anyhow!("no release tag in GitHub's redirect to {page}"))?
        .trim_start_matches('v')
        .to_string();

    Ok(is_newer(&version, current).then_some(Update { version, page }))
}

/// Pulls `v0.3.0` out of `https://github.com/o/r/releases/tag/v0.3.0`.
fn release_tag(location: &str) -> Option<&str> {
    let (before, tag) = location.rsplit_once("/releases/tag/")?;
    (!tag.is_empty() && !before.is_empty()).then_some(tag)
}

/// Compares dotted numeric versions.
///
/// A component that is not a plain number ends the comparison rather than
/// guessing what it means, so `0.3.0-rc1` compares as `0.3` — newer than
/// `0.2.0`, and never newer than the released `0.3.0`.
pub fn is_newer(latest: &str, current: &str) -> bool {
    numbers(latest) > numbers(current)
}

fn numbers(version: &str) -> Vec<u32> {
    version
        .trim_start_matches('v')
        .split('.')
        .map_while(|part| part.parse::<u32>().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_higher_number_anywhere_is_newer() {
        assert!(is_newer("0.3.0", "0.2.0"));
        assert!(is_newer("0.2.1", "0.2.0"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(is_newer("0.10.0", "0.9.0"), "10 is not 'less than' 9");
    }

    #[test]
    fn the_same_version_is_not_an_update() {
        assert!(!is_newer("0.2.0", "0.2.0"));
        assert!(
            !is_newer("v0.2.0", "0.2.0"),
            "the v prefix is not a version"
        );
    }

    #[test]
    fn an_older_release_never_reads_as_newer() {
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("0.2.0", "1.0.0"));
    }

    #[test]
    fn a_prerelease_suffix_stops_the_comparison_instead_of_confusing_it() {
        assert!(is_newer("0.3.0-rc1", "0.2.0"));
        assert!(!is_newer("0.3.0-rc1", "0.3.0"));
    }

    #[test]
    fn nonsense_from_the_api_is_not_an_update() {
        assert!(!is_newer("", "0.2.0"));
        assert!(!is_newer("latest", "0.2.0"));
    }

    #[test]
    fn the_tag_comes_out_of_the_redirect() {
        assert_eq!(
            release_tag("https://github.com/o/r/releases/tag/v0.3.0"),
            Some("v0.3.0")
        );
        assert_eq!(release_tag("https://github.com/o/r/releases"), None);
        assert_eq!(release_tag("https://github.com/o/r/releases/tag/"), None);
    }

    /// Ignored because it needs the network. Run it by hand when the endpoint
    /// or the reply shape might have moved:
    /// `cargo test -p jot-core -- --ignored reaches_github`.
    #[tokio::test]
    #[ignore]
    async fn reaches_github_and_parses_the_reply() {
        // 0.0.0 is older than anything that has ever been released, so a
        // successful call must come back with an update rather than None.
        let update = check("0.0.0").await.expect("GitHub should answer");
        let update = update.expect("every release is newer than 0.0.0");
        assert!(!update.version.is_empty());
        assert!(update.page.starts_with("https://github.com/"));
        assert!(!check("999.0.0").await.unwrap().is_some());
    }
}
