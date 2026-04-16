//! Fuzzy matching for commands, skills, themes, and file paths.
//!
//! Wraps `nucleo-matcher` for fast fuzzy string matching.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

/// A single fuzzy match result.
#[derive(Debug, Clone)]
pub struct FuzzyMatch<'a> {
    /// The matched item.
    pub item: &'a str,
    /// Matched character indices (for highlighting).
    pub matched_indices: Vec<u32>,
    /// Score — higher is better.
    pub score: u32,
}

/// Fuzzy matcher that can be reused across queries.
#[derive(Debug)]
pub struct FuzzyMatcher {
    case_matching: CaseMatching,
    normalization: Normalization,
}

impl FuzzyMatcher {
    /// Create a new fuzzy matcher with default settings.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            case_matching: CaseMatching::Smart,
            normalization: Normalization::Smart,
        }
    }

    /// Create a case-sensitive fuzzy matcher.
    #[must_use]
    pub const fn case_sensitive() -> Self {
        Self {
            case_matching: CaseMatching::Respect,
            normalization: Normalization::Smart,
        }
    }

    /// Find fuzzy matches for a pattern in a list of candidates.
    ///
    /// Returns matches sorted by score (highest first).
    #[must_use]
    pub fn fuzzy_match<'a>(
        &self,
        pattern: &str,
        candidates: &'a [impl AsRef<str>],
    ) -> Vec<FuzzyMatch<'a>> {
        if pattern.is_empty() {
            return candidates
                .iter()
                .map(|item| FuzzyMatch {
                    item: item.as_ref(),
                    matched_indices: Vec::new(),
                    score: 0,
                })
                .collect();
        }

        let mut matcher = Matcher::new(Config::DEFAULT);
        let nucleo_pattern = Pattern::parse(pattern, self.case_matching, self.normalization);
        let mut buf = Vec::new();

        let mut results: Vec<FuzzyMatch<'_>> = candidates
            .iter()
            .filter_map(|item| {
                let item_str = item.as_ref();
                let utf32 = Utf32Str::new(item_str, &mut buf);
                let mut matched_indices = Vec::new();
                let score = nucleo_pattern.indices(utf32, &mut matcher, &mut matched_indices);

                score.map(|s| {
                    matched_indices.sort_unstable();
                    matched_indices.dedup();
                    FuzzyMatch {
                        item: item_str,
                        matched_indices,
                        score: s,
                    }
                })
            })
            .collect();

        results.sort_by_key(|m| std::cmp::Reverse(m.score));
        results
    }

    /// Find the single best match for a pattern.
    #[must_use]
    pub fn best_match<'a>(
        &self,
        pattern: &str,
        candidates: &'a [impl AsRef<str>],
    ) -> Option<FuzzyMatch<'a>> {
        self.fuzzy_match(pattern, candidates).into_iter().next()
    }

    /// Filter a list of candidates, keeping only those matching the pattern.
    #[must_use]
    pub fn filter<'a>(&self, pattern: &str, candidates: &'a [impl AsRef<str>]) -> Vec<&'a str> {
        self.fuzzy_match(pattern, candidates)
            .into_iter()
            .map(|m| m.item)
            .collect()
    }
}

impl Default for FuzzyMatcher {
    fn default() -> Self {
        Self::new()
    }
}

/// Convenience function for one-off fuzzy matching.
#[must_use]
pub fn fuzzy_search<'a>(pattern: &str, candidates: &'a [impl AsRef<str>]) -> Vec<FuzzyMatch<'a>> {
    FuzzyMatcher::new().fuzzy_match(pattern, candidates)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_match_finds_partial_matches() {
        let candidates = ["main.rs", "lib.rs", "Cargo.toml", "README.md"];
        let results = fuzzy_search("main", &candidates);

        assert!(!results.is_empty());
        assert_eq!(results[0].item, "main.rs");
    }

    #[test]
    fn fuzzy_match_scores_ordered() {
        let candidates = [
            "src/components/Button.tsx",
            "src/utils/button_helpers.ts",
            "src/styles/buttons.css",
            "src/pages/HomePage.tsx",
        ];
        let results = fuzzy_search("btn", &candidates);

        assert!(!results.is_empty());
        // Scores should be in descending order
        for window in results.windows(2) {
            assert!(window[0].score >= window[1].score);
        }
    }

    #[test]
    fn fuzzy_match_empty_pattern_returns_all() {
        let candidates = ["a", "b", "c"];
        let results = fuzzy_search("", &candidates);

        assert_eq!(results.len(), 3);
    }

    #[test]
    fn fuzzy_match_no_matches_returns_empty() {
        let candidates = ["hello", "world"];
        let results = fuzzy_search("xyz123", &candidates);

        assert!(results.is_empty());
    }

    #[test]
    fn fuzzy_match_case_sensitive_mode() {
        let matcher = FuzzyMatcher::case_sensitive();
        let candidates = ["Foo", "foo", "FOO"];

        let results = matcher.fuzzy_match("Foo", &candidates);
        // Case sensitive: "Foo" should score highest
        assert!(!results.is_empty());
        assert_eq!(results[0].item, "Foo");
    }

    #[test]
    fn best_match_returns_top_result() {
        let candidates = ["index.html", "about.html", "contact.html"];
        let best = FuzzyMatcher::new().best_match("index", &candidates);

        assert!(best.is_some());
        assert_eq!(best.unwrap().item, "index.html");
    }

    #[test]
    fn matched_indices_for_highlighting() {
        let candidates = ["hello_world.rs"];
        let results = fuzzy_search("hwr", &candidates);

        assert!(!results.is_empty());
        assert!(!results[0].matched_indices.is_empty());
    }
}
