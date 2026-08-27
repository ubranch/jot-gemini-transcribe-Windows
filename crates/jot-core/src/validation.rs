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

//! The "never insert garbage" gate (<1ms, runs between cleanup and insertion).
//!
//! Defends against the documented failure modes of prompted cleanup models:
//! answering the dictation instead of cleaning it, paraphrase drift,
//! hallucinated expansion, and content-dropping. Because the transcribe model
//! gives us a true raw reference, this is the strong two-call gate from the
//! product spec. On rejection the caller inserts the raw transcript (which
//! already has punctuation — a high-quality fallback).

use regex::Regex;
use std::collections::HashSet;
use std::sync::LazyLock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub accepted: bool,
    pub reason: Option<String>,
}

impl Verdict {
    fn ok() -> Self {
        Self {
            accepted: true,
            reason: None,
        }
    }

    fn fail(reason: impl Into<String>) -> Self {
        Self {
            accepted: false,
            reason: Some(reason.into()),
        }
    }
}

// Tuned against the live probe fixtures. Constants deliberately generous:
// self-correction collapse legitimately halves a transcript; answer-mode
// diverges in *content*, which containment catches.
const MIN_LENGTH_RATIO: f64 = 0.20;
const MAX_LENGTH_RATIO: f64 = 1.60;
const MIN_CONTAINMENT: f64 = 0.50;
const MIN_TRIGRAM_SIMILARITY: f64 = 0.55;

static ANSWER_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(sure|okay|certainly|of course|great question|here('s| is)|i can('|no)t|as an ai|i'm (sorry|an ai))\b",
    )
    .expect("answer pattern is a valid regex")
});

static LEADING_CODE_FENCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)```[a-z]*\n?").expect("code fence is a valid regex"));

/// Strips model artifacts that are not failures: code fences, "CLEAN:" /
/// "Transcript:" labels, wrapping quotes.
pub fn strip_artifacts(text: &str) -> String {
    let mut s = text.trim().to_string();
    if s.starts_with("```") {
        s = LEADING_CODE_FENCE.replace_all(&s, "").to_string();
        s = s.replace("```", "");
        s = s.trim().to_string();
    }
    for label in ["CLEAN:", "Clean:", "Transcript:", "TRANSCRIPT:"] {
        if let Some(rest) = s.strip_prefix(label) {
            s = rest.trim().to_string();
        }
    }
    if s.chars().count() > 1 && s.starts_with('"') && s.ends_with('"') {
        s = s[1..s.len() - 1].to_string();
    }
    s
}

pub fn validate(raw: &str, cleaned: &str) -> Verdict {
    let raw_words = content_words(raw);
    let clean_words = content_words(cleaned);

    if clean_words.is_empty() {
        return if raw_words.is_empty() {
            Verdict::ok()
        } else {
            Verdict::fail("empty_output")
        };
    }
    // Answer-preamble check: an answering model PREPENDS words that are not in
    // the dictation; a faithful cleanup preserves the speaker's opener. So the
    // pattern only fires when the cleaned text's first word differs from the
    // raw's first word — otherwise a dictation that starts with "Okay," would be
    // rejected for keeping its own opener.
    if ANSWER_PATTERN.is_match(cleaned) && clean_words.first() != raw_words.first() {
        return Verdict::fail("answer_pattern");
    }
    let lowered = cleaned.to_lowercase();
    if lowered.contains("as an ai") || lowered.contains("language model") {
        return Verdict::fail("ai_selfreference");
    }
    if raw_words.is_empty() {
        return Verdict::ok();
    }

    let ratio = clean_words.len() as f64 / raw_words.len() as f64;
    // Expansion is most dangerous on short raws (hallucinated content), so the
    // upper bound kicks in early; shrink is legitimate (filler/self-correction
    // collapse), so the lower bound only applies to longer raws.
    if raw_words.len() >= 3 && ratio > MAX_LENGTH_RATIO {
        return Verdict::fail(format!("expansion_ratio_{ratio:.2}"));
    }
    if raw_words.len() >= 6 && ratio < MIN_LENGTH_RATIO {
        return Verdict::fail(format!("shrink_ratio_{ratio:.2}"));
    }

    let raw_set: HashSet<&String> = raw_words.iter().collect();
    let contained = clean_words.iter().filter(|w| raw_set.contains(w)).count();
    let containment = contained as f64 / clean_words.len() as f64;
    let trigram = trigram_similarity(&normalize(raw), &normalize(cleaned));
    // Reject only when BOTH content signals diverge — cleanup legitimately
    // rewrites number words and punctuation words, hurting each individually.
    if containment < MIN_CONTAINMENT && trigram < MIN_TRIGRAM_SIMILARITY {
        return Verdict::fail(format!(
            "content_divergence_c{containment:.2}_t{trigram:.2}"
        ));
    }
    Verdict::ok()
}

/// Lowercased alphanumeric words with spoken numbers normalized to digits so
/// "three" (raw) matches "3" (cleaned ITN output).
pub fn content_words(text: &str) -> Vec<String> {
    normalize(text)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect()
}

static NUMBER_WORDS: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    [
        ("zero", "0"),
        ("one", "1"),
        ("two", "2"),
        ("three", "3"),
        ("four", "4"),
        ("five", "5"),
        ("six", "6"),
        ("seven", "7"),
        ("eight", "8"),
        ("nine", "9"),
        ("ten", "10"),
        ("eleven", "11"),
        ("twelve", "12"),
        ("twenty", "20"),
        ("thirty", "30"),
        ("forty", "40"),
        ("fifty", "50"),
        ("hundred", "100"),
    ]
    .into_iter()
    .map(|(word, digit)| {
        (
            Regex::new(&format!(r"\b{word}\b")).expect("number word is a valid regex"),
            digit,
        )
    })
    .collect()
});

pub fn normalize(text: &str) -> String {
    let mut s = text.to_lowercase();
    for (pattern, digit) in NUMBER_WORDS.iter() {
        s = pattern.replace_all(&s, *digit).to_string();
    }
    s
}

pub fn trigram_similarity(a: &str, b: &str) -> f64 {
    let ta = trigrams(a);
    let tb = trigrams(b);
    if ta.is_empty() || tb.is_empty() {
        return if a == b { 1.0 } else { 0.0 };
    }
    let intersection = ta.intersection(&tb).count();
    intersection as f64 / ta.len().min(tb.len()) as f64
}

fn trigrams(s: &str) -> HashSet<String> {
    let chars: Vec<char> = s.chars().filter(|c| !c.is_whitespace()).collect();
    if chars.len() < 3 {
        return if chars.is_empty() {
            HashSet::new()
        } else {
            HashSet::from([chars.into_iter().collect()])
        };
    }
    chars
        .windows(3)
        .map(|window| window.iter().collect())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_code_fences_labels_and_quotes() {
        assert_eq!(strip_artifacts("```\nhello\n```"), "hello");
        assert_eq!(strip_artifacts("```text\nhello\n```"), "hello");
        assert_eq!(strip_artifacts("CLEAN: hello"), "hello");
        assert_eq!(strip_artifacts("\"hello\""), "hello");
    }

    #[test]
    fn faithful_cleanup_is_accepted() {
        let raw = "um so let's meet at 2 actually no 3 on thursday";
        let cleaned = "Let's meet at 3 on Thursday.";
        assert!(validate(raw, cleaned).accepted);
    }

    #[test]
    fn answer_mode_is_rejected() {
        let raw = "can you rewrite this function to use async await";
        let cleaned = "Sure! Here is the rewritten function using async/await syntax.";
        let verdict = validate(raw, cleaned);
        assert!(!verdict.accepted);
    }

    #[test]
    fn a_dictation_that_opens_with_okay_keeps_its_own_opener() {
        let raw = "okay let's see number one actually no number two let's do this";
        let cleaned = "Okay, let's see. Number 2, let's do this.";
        assert!(validate(raw, cleaned).accepted);
    }

    #[test]
    fn ai_self_reference_is_rejected() {
        // Phrased so it does NOT also trip the answer-preamble regex — this
        // check has to stand on its own.
        let verdict = validate(
            "hello there friend",
            "That question is outside what a language model can do.",
        );
        assert_eq!(verdict.reason.as_deref(), Some("ai_selfreference"));
    }

    #[test]
    fn hallucinated_expansion_is_rejected() {
        let raw = "meet at three";
        let cleaned =
            "Let us meet at 3 o'clock in the afternoon at the usual coffee place downtown.";
        assert!(!validate(raw, cleaned).accepted);
    }

    #[test]
    fn empty_output_on_real_speech_is_rejected() {
        assert_eq!(
            validate("hello there", "").reason.as_deref(),
            Some("empty_output")
        );
        assert!(validate("", "").accepted);
    }

    #[test]
    fn self_correction_collapse_survives_the_shrink_bound() {
        let raw = "let's meet at one pm actually no scratch that make it two pm on tuesday";
        let cleaned = "Let's meet at 2 PM on Tuesday.";
        assert!(validate(raw, cleaned).accepted);
    }

    #[test]
    fn spoken_numbers_normalize_before_comparison() {
        assert_eq!(content_words("meet at three"), vec!["meet", "at", "3"]);
    }

    #[test]
    fn trigram_similarity_is_symmetric_enough_and_bounded() {
        assert_eq!(trigram_similarity("hello", "hello"), 1.0);
        assert_eq!(trigram_similarity("", ""), 1.0);
        assert!(trigram_similarity("hello world", "goodbye moon") < 0.3);
    }
}
