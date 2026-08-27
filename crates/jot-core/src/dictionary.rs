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

//! The personal dictionary: terms (spelling hints fed to the recogniser and the
//! cleanup prompt) and explicit wrong→right rules (enforced deterministically
//! post-model).

use crate::file_layout;
use crate::replacement::Rule;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use time::OffsetDateTime;
use uuid::Uuid;

/// Terms outside this length are rejected on entry and on import.
pub const TERM_LENGTH: std::ops::RangeInclusive<usize> = 1..=60;
/// Ceiling on a single CSV import, so a pasted spreadsheet can't wedge the app.
pub const MAX_IMPORT_ROWS: usize = 1000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DictionaryEntry {
    pub id: Uuid,
    /// The correct term ("Kubernetes", "Ammaar", "gRPC"). 1–60 chars.
    pub term: String,
    /// Optional misspelling the model tends to produce ("cooper netties").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub misspelling: Option<String>,
    #[serde(default)]
    pub starred: bool,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl DictionaryEntry {
    pub fn new(term: impl Into<String>, misspelling: Option<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            term: term.into(),
            misspelling: misspelling.filter(|m| !m.is_empty()),
            starred: false,
            created_at: OffsetDateTime::now_utc(),
        }
    }
}

pub struct DictionaryStore {
    path: PathBuf,
    entries: RwLock<Vec<DictionaryEntry>>,
}

static GLOBAL: LazyLock<Arc<DictionaryStore>> =
    LazyLock::new(|| Arc::new(DictionaryStore::open(file_layout::dictionary_json())));

impl DictionaryStore {
    pub fn global() -> Arc<DictionaryStore> {
        GLOBAL.clone()
    }

    pub fn open(path: PathBuf) -> Self {
        let entries = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self {
            path,
            entries: RwLock::new(entries),
        }
    }

    pub fn entries(&self) -> Vec<DictionaryEntry> {
        self.entries.read().clone()
    }

    pub fn save(&self, entries: Vec<DictionaryEntry>) {
        *self.entries.write() = entries;
        self.persist();
    }

    fn persist(&self) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let Ok(encoded) = serde_json::to_vec_pretty(&*self.entries.read()) else {
            tracing::error!("DictionaryStore: encode failed");
            return;
        };
        let temp = self.path.with_extension("json.tmp");
        if std::fs::write(&temp, &encoded).is_ok()
            && let Err(error) = std::fs::rename(&temp, &self.path)
        {
            tracing::error!(%error, "DictionaryStore: rename failed");
            let _ = std::fs::remove_file(&temp);
        }
    }

    /// Returns false when the term is out of range or already present.
    pub fn add(&self, term: &str, misspelling: Option<&str>) -> bool {
        let trimmed = term.trim();
        if !TERM_LENGTH.contains(&trimmed.chars().count()) {
            return false;
        }
        let mut entries = self.entries.write();
        if entries
            .iter()
            .any(|e| e.term.to_lowercase() == trimmed.to_lowercase())
        {
            return false;
        }
        entries.push(DictionaryEntry::new(
            trimmed,
            misspelling.map(|m| m.trim().to_string()),
        ));
        drop(entries);
        self.persist();
        true
    }

    pub fn remove(&self, id: Uuid) {
        self.entries.write().retain(|entry| entry.id != id);
        self.persist();
    }

    pub fn toggle_star(&self, id: Uuid) {
        {
            let mut entries = self.entries.write();
            if let Some(entry) = entries.iter_mut().find(|entry| entry.id == id) {
                entry.starred = !entry.starred;
            }
        }
        self.persist();
    }

    // ----- Pipeline inputs

    /// Starred first, then newest.
    fn ranked(&self) -> Vec<DictionaryEntry> {
        let mut entries = self.entries();
        entries.sort_by(|a, b| {
            b.starred
                .cmp(&a.starred)
                .then(b.created_at.cmp(&a.created_at))
        });
        entries
    }

    /// Vocabulary for the cleanup prompt: starred first, then newest. Cap 100.
    pub fn vocabulary(&self) -> Vec<String> {
        self.ranked()
            .into_iter()
            .take(100)
            .map(|entry| entry.term)
            .collect()
    }

    /// Vocabulary as it goes over the wire — the SHARED sanitizer for both the
    /// cleanup prompt and the transcription request's `custom_vocabulary`.
    ///
    /// Dictionary entries are user/CSV data, so newlines are stripped and each
    /// term capped: a crafted entry must not be able to smuggle its own
    /// instruction line into the prompt. Both consumers call this so they cannot
    /// drift apart, and a total-byte ceiling bounds the request whatever the
    /// per-term caps allow. Only CORRECT terms — never misspellings; biasing a
    /// recogniser toward "cooper netties" is actively harmful.
    pub fn sanitized_vocabulary(&self, max_bytes: usize) -> Vec<String> {
        let mut used = 0;
        let mut out = Vec::new();
        for term in self.vocabulary() {
            let clean: String = term
                .replace(['\n', '\r'], " ")
                .chars()
                .take(60)
                .collect::<String>()
                .trim()
                .to_string();
            if clean.is_empty() {
                continue;
            }
            let cost = clean.len() + 1;
            if used + cost > max_bytes {
                break; // truncate from the END: starred survive
            }
            used += cost;
            out.push(clean);
        }
        out
    }

    /// Spelling hints for the prompt (top 10 with misspellings) — starred first,
    /// matching `vocabulary()`: "starred words are prioritized" must be true for
    /// both prompt inputs, not just one.
    pub fn spellings(&self) -> Vec<(String, String)> {
        self.ranked()
            .into_iter()
            .filter_map(|entry| {
                entry
                    .misspelling
                    .filter(|m| !m.is_empty())
                    .map(|wrong| (wrong, entry.term))
            })
            .take(10)
            .collect()
    }

    /// Deterministic rules for the replacement engine (ALL entries with misspellings).
    pub fn replacement_rules(&self) -> Vec<Rule> {
        self.entries()
            .into_iter()
            .filter_map(|entry| {
                entry
                    .misspelling
                    .filter(|m| !m.is_empty())
                    .map(|wrong| Rule::new(wrong, entry.term))
            })
            .collect()
    }

    // ----- CSV (data portability)

    pub fn export_csv(&self) -> String {
        let mut lines = vec!["term,misspelling".to_string()];
        for entry in self.entries() {
            let term = entry.term.replace('"', "\"\"");
            let misspelling = entry.misspelling.unwrap_or_default().replace('"', "\"\"");
            lines.push(format!("\"{term}\",\"{misspelling}\""));
        }
        lines.join("\n")
    }

    pub fn import_csv(&self, csv: &str) -> usize {
        let mut records = split_records(csv);
        // Only drop the first line when its first CELL is exactly a header word —
        // a `starts_with("term")` check would eat a real first entry like
        // "terminal" from a headerless file.
        if let Some(first) = records.first() {
            let first_cell = parse_csv_line(first)
                .first()
                .map(|cell| cell.to_lowercase())
                .unwrap_or_default();
            if ["term", "word", "phrase"].contains(&first_cell.as_str()) {
                records.remove(0);
            }
        }

        let mut entries = self.entries.write();
        let mut seen: HashSet<String> = entries.iter().map(|e| e.term.to_lowercase()).collect();
        let mut imported = 0;
        for record in records {
            if imported >= MAX_IMPORT_ROWS {
                break;
            }
            let columns = parse_csv_line(&record);
            let Some(term) = columns.first().map(|c| c.trim().to_string()) else {
                continue;
            };
            if !TERM_LENGTH.contains(&term.chars().count()) || seen.contains(&term.to_lowercase()) {
                continue;
            }
            let misspelling = columns
                .get(1)
                .filter(|column| !column.is_empty())
                .map(|column| column.to_string());
            seen.insert(term.to_lowercase());
            entries.push(DictionaryEntry::new(term, misspelling));
            imported += 1;
        }
        drop(entries);
        if imported > 0 {
            self.persist();
        }
        imported
    }
}

/// Quote-aware record splitter: CRLF endings and RFC-4180 quoted newlines both
/// break a naive `split('\n')` (dropped or mangled rows while reporting success).
fn split_records(csv: &str) -> Vec<String> {
    let mut records = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for char in csv.chars() {
        match char {
            '"' => {
                in_quotes = !in_quotes;
                current.push(char);
            }
            '\n' | '\r' if !in_quotes => {
                if !current.is_empty() {
                    records.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(char),
        }
    }
    if !current.is_empty() {
        records.push(current);
    }
    records
}

fn parse_csv_line(line: &str) -> Vec<String> {
    let mut columns = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let chars: Vec<char> = line.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        match (chars[index], in_quotes) {
            // RFC 4180 escaped quote.
            ('"', true) if chars.get(index + 1) == Some(&'"') => {
                current.push('"');
                index += 1;
            }
            ('"', _) => in_quotes = !in_quotes,
            (',', false) => columns.push(std::mem::take(&mut current)),
            (char, _) => current.push(char),
        }
        index += 1;
    }
    columns.push(current);
    columns
        .into_iter()
        .map(|column| column.trim().to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, DictionaryStore) {
        let temp = tempfile::tempdir().unwrap();
        let store = DictionaryStore::open(temp.path().join("dictionary.json"));
        (temp, store)
    }

    #[test]
    fn add_rejects_duplicates_and_out_of_range_terms() {
        let (_temp, store) = store();
        assert!(store.add("Kubernetes", Some("cooper netties")));
        assert!(!store.add("kubernetes", None), "case-insensitive duplicate");
        assert!(!store.add("", None));
        assert!(!store.add(&"x".repeat(61), None));
        assert_eq!(store.entries().len(), 1);
    }

    #[test]
    fn entries_persist_across_reopen() {
        let (temp, store) = store();
        store.add("gRPC", None);
        let reopened = DictionaryStore::open(temp.path().join("dictionary.json"));
        assert_eq!(reopened.entries()[0].term, "gRPC");
    }

    #[test]
    fn starred_terms_rank_first_in_both_prompt_inputs() {
        let (_temp, store) = store();
        store.add("First", Some("furst"));
        store.add("Second", Some("secund"));
        let id = store.entries()[0].id;
        store.toggle_star(id);

        assert_eq!(store.vocabulary()[0], "First");
        assert_eq!(store.spellings()[0], ("furst".into(), "First".into()));
    }

    #[test]
    fn sanitized_vocabulary_strips_newlines_and_honours_the_byte_ceiling() {
        let (_temp, store) = store();
        store.add("Alpha\nBeta", None);
        assert_eq!(store.sanitized_vocabulary(2048), vec!["Alpha Beta"]);
        // A ceiling below the first term's cost yields nothing rather than a
        // truncated term.
        assert!(store.sanitized_vocabulary(4).is_empty());
    }

    #[test]
    fn replacement_rules_skip_entries_without_a_misspelling() {
        let (_temp, store) = store();
        store.add("Kubernetes", Some("cooper netties"));
        store.add("Ammaar", None);
        let rules = store.replacement_rules();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].right, "Kubernetes");
    }

    #[test]
    fn csv_round_trips() {
        let (_temp, source) = store();
        source.add("gRPC", Some("g r p c"));
        source.add("Quote\"Term", None);

        let (_temp2, target) = store();
        assert_eq!(target.import_csv(&source.export_csv()), 2);
        let terms: Vec<String> = target.entries().into_iter().map(|e| e.term).collect();
        assert!(terms.contains(&"gRPC".to_string()));
        assert!(terms.contains(&"Quote\"Term".to_string()));
    }

    #[test]
    fn import_handles_crlf_and_headerless_files() {
        let (_temp, store) = store();
        assert_eq!(store.import_csv("terminal,terminul\r\nKubernetes,\r\n"), 2);
        let terms: Vec<String> = store.entries().into_iter().map(|e| e.term).collect();
        assert!(terms.contains(&"terminal".to_string()), "{terms:?}");
    }

    #[test]
    fn import_drops_only_a_real_header_row() {
        let (_temp, store) = store();
        assert_eq!(
            store.import_csv("term,misspelling\nKubernetes,cooper netties"),
            1
        );
        assert_eq!(store.entries().len(), 1);
        assert_eq!(store.entries()[0].term, "Kubernetes");
    }

    #[test]
    fn import_skips_duplicates_within_and_across_the_file() {
        let (_temp, store) = store();
        store.add("gRPC", None);
        assert_eq!(store.import_csv("gRPC,\ngrpc,\nNew,"), 1);
    }

    #[test]
    fn quoted_newlines_do_not_split_a_record() {
        assert_eq!(split_records("\"a\nb\",c\nd,e").len(), 2);
    }
}
