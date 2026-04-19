//! Search — terminal text search with highlight matching.

use regex::{Regex, RegexBuilder};

/// A search result — a match within a line.
#[derive(Debug, Clone)]
pub struct SearchMatch {
    /// Line index.
    pub line: usize,
    /// Start column (byte offset).
    pub start: usize,
    /// End column (byte offset).
    pub end: usize,
    /// The matched text.
    pub text: String,
}

/// Search state for the current session.
#[derive(Debug)]
pub struct SearchState {
    /// Current search query.
    pub query: String,
    /// Compiled regex (if query is non-empty and valid).
    regex: Option<Regex>,
    /// All matches in the current document.
    matches: Vec<SearchMatch>,
    /// Index of the currently highlighted match.
    active_index: usize,
}

impl Default for SearchState {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchState {
    /// Create a new search state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            query: String::new(),
            regex: None,
            matches: Vec::new(),
            active_index: 0,
        }
    }

    /// Set the search query and recompute matches.
    pub fn set_query(&mut self, query: &str) {
        self.query = query.to_string();
        self.regex = if query.is_empty() {
            None
        } else {
            RegexBuilder::new(&regex::escape(query))
                .case_insensitive(true)
                .build()
                .ok()
        };
        self.active_index = 0;
        // Recompute matches on next call to search_in()
    }

    /// Search within a set of lines, returning all matches.
    #[must_use]
    pub fn search_in(&mut self, lines: &[&str]) -> &[SearchMatch] {
        self.matches.clear();
        if let Some(ref re) = self.regex {
            for (line_idx, line) in lines.iter().enumerate() {
                for m in re.find_iter(line) {
                    self.matches.push(SearchMatch {
                        line: line_idx,
                        start: m.start(),
                        end: m.end(),
                        text: m.as_str().to_string(),
                    });
                }
            }
        }
        self.active_index = 0;
        &self.matches
    }

    /// Go to the next match.
    pub fn next(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        self.active_index = (self.active_index + 1) % self.matches.len();
    }

    /// Go to the previous match.
    pub fn prev(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        self.active_index = if self.active_index == 0 {
            self.matches.len() - 1
        } else {
            self.active_index - 1
        };
    }

    /// Get the active match.
    #[must_use]
    pub fn active(&self) -> Option<&SearchMatch> {
        self.matches.get(self.active_index)
    }

    /// Total match count.
    #[must_use]
    pub fn match_count(&self) -> usize {
        self.matches.len()
    }

    /// Clear the search.
    pub fn clear(&mut self) {
        self.query.clear();
        self.regex = None;
        self.matches.clear();
        self.active_index = 0;
    }

    /// Check if currently searching.
    #[must_use]
    pub fn is_active(&self) -> bool {
        !self.matches.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_all_matches() {
        let mut state = SearchState::new();
        state.set_query("hello");
        let lines = vec!["hello world", "say hello", "no match here", "hello"];
        let matches = state.search_in(&lines);
        assert_eq!(matches.len(), 3);
        assert_eq!(matches[0].line, 0);
        assert_eq!(matches[1].line, 1);
        assert_eq!(matches[2].line, 3);
    }

    #[test]
    fn case_insensitive() {
        let mut state = SearchState::new();
        state.set_query("HELLO");
        let lines = vec!["hello world", "Hello there"];
        let matches = state.search_in(&lines);
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn next_prev_wrap() {
        let mut state = SearchState::new();
        state.set_query("x");
        let lines = vec!["x", "x", "x"];
        let _ = state.search_in(&lines);
        assert_eq!(state.active_index, 0);

        state.next();
        assert_eq!(state.active_index, 1);
        state.next();
        assert_eq!(state.active_index, 2);
        state.next();
        assert_eq!(state.active_index, 0); // wraps

        state.prev();
        assert_eq!(state.active_index, 2);
    }

    #[test]
    fn empty_query_no_matches() {
        let mut state = SearchState::new();
        state.set_query("");
        let lines = vec!["hello world"];
        let matches = state.search_in(&lines);
        assert!(matches.is_empty());
    }

    #[test]
    fn clear_removes_matches() {
        let mut state = SearchState::new();
        state.set_query("hello");
        let _ = state.search_in(&["hello"]);
        assert!(state.is_active());
        state.clear();
        assert!(!state.is_active());
    }

    #[test]
    fn special_chars_escaped() {
        let mut state = SearchState::new();
        state.set_query("[test]");
        let lines = vec!["[test] foo", "test"]; // should only match literal [test]
        let matches = state.search_in(&lines);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].line, 0);
    }
}
