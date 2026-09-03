use super::tree::MAX_TREE_PATH_BYTES;
use std::collections::BTreeSet;
use std::fmt;

/// The largest ignore pattern accepted by a backup or Live request.
pub const MAX_IGNORE_RULE_LENGTH: usize = 4 * 1024;

/// The largest number of ignore patterns accepted by one request.
pub const MAX_IGNORE_RULES: usize = 1_024;

/// The default policy is to exclude Git metadata from captured paths.
pub const DEFAULT_IGNORE_GIT: bool = true;

/// A validated, normalized ignore pattern.
///
/// Patterns use `/` as the portable path separator. A pattern without a
/// separator is a name pattern and is matched against every path component.
/// A pattern containing a separator is anchored at the source root. `*` and
/// `?` match characters within one component; a component written as `**`
/// matches zero or more complete components.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct IgnorePattern {
    value: String,
    components: Vec<String>,
    path_pattern: bool,
    recursive: bool,
}

impl IgnorePattern {
    /// Parses and normalizes one name or relative-path pattern.
    pub fn new(value: impl AsRef<str>) -> Result<Self, IgnorePatternError> {
        let value = value.as_ref();
        let normalized = normalize_pattern(value)?;
        let components = normalized.split('/').map(str::to_owned).collect::<Vec<_>>();
        let recursive = components.iter().any(|component| component == "**");
        Ok(Self {
            path_pattern: components.len() > 1,
            value: normalized,
            components,
            recursive,
        })
    }

    /// Returns the canonical slash-separated pattern.
    pub fn as_str(&self) -> &str {
        &self.value
    }

    /// Returns whether this pattern matches a name at any depth.
    pub const fn is_name_pattern(&self) -> bool {
        !self.path_pattern
    }

    /// Returns whether this pattern is anchored at the source root.
    pub const fn is_path_pattern(&self) -> bool {
        self.path_pattern
    }

    fn matches_normalized_path(&self, path: &str) -> bool {
        let components = path.split('/').collect::<Vec<_>>();
        if self.path_pattern {
            self.matches_path_prefix(&components)
        } else {
            components
                .iter()
                .any(|component| component_matches(&self.components[0], component))
        }
    }

    fn matches_path_prefix(&self, path: &[&str]) -> bool {
        if !self.recursive {
            return path.len() >= self.components.len()
                && self
                    .components
                    .iter()
                    .zip(path.iter())
                    .all(|(pattern, component)| component_matches(pattern, component));
        }

        let pattern_length = self.components.len();
        let mut states = vec![false; pattern_length + 1];
        states[0] = true;
        recursive_closure(&mut states, &self.components);

        for component in path {
            let mut next = vec![false; pattern_length + 1];
            for (index, pattern) in self.components.iter().enumerate() {
                if !states[index] {
                    continue;
                }
                if pattern == "**" {
                    next[index] = true;
                } else if component_matches(pattern, component) {
                    next[index + 1] = true;
                }
            }
            recursive_closure(&mut next, &self.components);
            if next[pattern_length] {
                return true;
            }
            states = next;
        }
        false
    }
}

impl AsRef<str> for IgnorePattern {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for IgnorePattern {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A malformed ignore pattern supplied by configuration or a request.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum IgnorePatternError {
    /// The pattern is empty or contains only whitespace.
    Empty,
    /// The pattern exceeds [`MAX_IGNORE_RULE_LENGTH`].
    TooLong,
    /// The request contains more than [`MAX_IGNORE_RULES`] patterns.
    TooManyRules,
    /// The pattern contains a NUL byte.
    Nul,
    /// The pattern is absolute rather than relative to the capture root.
    Absolute,
    /// The pattern contains an empty path component.
    EmptyComponent,
    /// The pattern contains `.` or `..` traversal components.
    Traversal,
    /// The pattern contains a control character or platform-ambiguous
    /// punctuation.
    InvalidCharacter,
}

impl fmt::Display for IgnorePatternError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Empty => "ignore pattern must contain at least one non-whitespace character",
            Self::TooLong => "ignore pattern exceeds the configured length limit",
            Self::TooManyRules => "ignore request contains too many patterns",
            Self::Nul => "ignore pattern must not contain a NUL byte",
            Self::Absolute => "ignore pattern must be relative to the capture root",
            Self::EmptyComponent => "ignore pattern must not contain an empty path component",
            Self::Traversal => "ignore pattern must not contain a traversal component",
            Self::InvalidCharacter => {
                "ignore pattern contains a control character or platform-ambiguous character"
            }
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for IgnorePatternError {}

/// A malformed relative path passed to an ignore-policy diagnostic.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum IgnorePathError {
    /// The path exceeds the portable relative-path limit.
    TooLong,
    /// The path is absolute rather than relative to the capture root.
    Absolute,
    /// The path contains an empty component or trailing separator.
    EmptyComponent,
    /// The path contains `.` or `..` traversal components.
    Traversal,
    /// The path contains a NUL, control character, or portable-tree punctuation.
    InvalidCharacter,
}

impl fmt::Display for IgnorePathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::TooLong => "relative path exceeds the portable path limit",
            Self::Absolute => "relative path must not be absolute",
            Self::EmptyComponent => "relative path must not contain an empty component",
            Self::Traversal => "relative path must not contain a traversal component",
            Self::InvalidCharacter => "relative path contains an invalid control character",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for IgnorePathError {}

/// Explains why a path was excluded.
#[non_exhaustive]
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum IgnoreMatch {
    /// A user-supplied name or path pattern matched.
    Pattern(IgnorePattern),
    /// The path contains a `.git` component covered by the built-in rule.
    GitPath,
}

impl IgnoreMatch {
    /// Returns the user pattern when this was a pattern match.
    pub fn pattern(&self) -> Option<&IgnorePattern> {
        match self {
            Self::Pattern(pattern) => Some(pattern),
            Self::GitPath => None,
        }
    }

    /// Returns whether the built-in Git exclusion caused the match.
    pub const fn is_git_path(&self) -> bool {
        matches!(self, Self::GitPath)
    }
}

/// The result of evaluating one normalized relative path.
///
/// The decision contains only the normalized relative path and the rule that
/// matched. It deliberately never stores or formats the source root.
#[non_exhaustive]
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum IgnoreDecision {
    /// The path is included by this policy.
    Included {
        /// The normalized path relative to the capture root.
        path: String,
    },
    /// The path and all of its descendants are excluded by this policy.
    Ignored {
        /// The normalized path relative to the capture root.
        path: String,
        /// The rule that caused the exclusion.
        matched: IgnoreMatch,
    },
}

impl IgnoreDecision {
    /// Returns the normalized relative path evaluated by the policy.
    pub fn path(&self) -> &str {
        match self {
            Self::Included { path } | Self::Ignored { path, .. } => path,
        }
    }

    /// Returns whether this path is excluded.
    pub const fn is_ignored(&self) -> bool {
        matches!(self, Self::Ignored { .. })
    }

    /// Returns the matching rule when the path is excluded.
    pub fn matched(&self) -> Option<&IgnoreMatch> {
        match self {
            Self::Included { .. } => None,
            Self::Ignored { matched, .. } => Some(matched),
        }
    }

    /// Returns the matching user pattern, if any.
    pub fn matched_pattern(&self) -> Option<&IgnorePattern> {
        self.matched().and_then(IgnoreMatch::pattern)
    }
}

/// The reusable capture-selection policy shared by Backup and Live.
///
/// User patterns are sorted and deduplicated after separator normalization.
/// The built-in Git rule is enabled by default and is independent of user
/// patterns, so disabling it does not remove an explicit user rule for `.git`.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct IgnorePolicy {
    patterns: Vec<IgnorePattern>,
    ignore_git: bool,
}

impl IgnorePolicy {
    /// Creates a policy with the supplied patterns and the default Git rule.
    pub fn new<I, T>(patterns: I) -> Result<Self, IgnorePatternError>
    where
        I: IntoIterator<Item = T>,
        T: AsRef<str>,
    {
        let mut unique = BTreeSet::new();
        for (index, pattern) in patterns.into_iter().enumerate() {
            if index >= MAX_IGNORE_RULES {
                return Err(IgnorePatternError::TooManyRules);
            }
            unique.insert(IgnorePattern::new(pattern)?);
        }
        Ok(Self {
            patterns: unique.into_iter().collect(),
            ignore_git: DEFAULT_IGNORE_GIT,
        })
    }

    /// Alias for [`Self::new`].
    pub fn from_patterns<I, T>(patterns: I) -> Result<Self, IgnorePatternError>
    where
        I: IntoIterator<Item = T>,
        T: AsRef<str>,
    {
        Self::new(patterns)
    }

    /// Creates a policy with an explicit built-in Git setting.
    pub fn from_patterns_with_git_ignored<I, T>(
        patterns: I,
        ignore_git: bool,
    ) -> Result<Self, IgnorePatternError>
    where
        I: IntoIterator<Item = T>,
        T: AsRef<str>,
    {
        Self::new(patterns).map(|policy| policy.with_ignore_git(ignore_git))
    }

    /// Creates a policy that includes Git paths unless a user pattern excludes
    /// them.
    pub fn including_git<I, T>(patterns: I) -> Result<Self, IgnorePatternError>
    where
        I: IntoIterator<Item = T>,
        T: AsRef<str>,
    {
        Self::new(patterns).map(Self::with_no_ignore_git)
    }

    /// Enables or disables the built-in Git exclusion.
    pub const fn with_ignore_git(mut self, ignore_git: bool) -> Self {
        self.ignore_git = ignore_git;
        self
    }

    /// Disables the built-in Git exclusion, equivalent to `--no-ignore-git`.
    pub const fn with_no_ignore_git(self) -> Self {
        self.with_ignore_git(false)
    }

    /// Returns the normalized, sorted, deduplicated user patterns.
    pub fn patterns(&self) -> &[IgnorePattern] {
        &self.patterns
    }

    /// Returns the normalized user pattern strings in deterministic order.
    pub fn pattern_strings(&self) -> impl Iterator<Item = &str> {
        self.patterns.iter().map(IgnorePattern::as_str)
    }

    /// Returns whether the built-in Git exclusion is enabled.
    pub const fn ignores_git(&self) -> bool {
        self.ignore_git
    }

    /// Returns whether Git paths may be included by this policy.
    pub const fn includes_git(&self) -> bool {
        !self.ignore_git
    }

    /// Evaluates one relative path and returns a source-root-safe diagnostic.
    pub fn decision<P>(&self, path: P) -> Result<IgnoreDecision, IgnorePathError>
    where
        P: AsRef<str>,
    {
        let path = normalize_relative_path(path.as_ref())?;
        if path.is_empty() {
            return Ok(IgnoreDecision::Included { path });
        }
        if self.ignore_git && is_git_path(&path) {
            return Ok(IgnoreDecision::Ignored {
                path,
                matched: IgnoreMatch::GitPath,
            });
        }
        if let Some(pattern) = self
            .patterns
            .iter()
            .find(|pattern| pattern.matches_normalized_path(&path))
        {
            return Ok(IgnoreDecision::Ignored {
                path,
                matched: IgnoreMatch::Pattern(pattern.clone()),
            });
        }
        Ok(IgnoreDecision::Included { path })
    }

    /// Returns whether a relative path is excluded.
    ///
    /// Invalid diagnostic paths return `false`; callers that need to
    /// distinguish invalid input should use [`Self::decision`]. Scanner paths
    /// are already validated before this method is called.
    pub fn is_ignored<P>(&self, path: P) -> bool
    where
        P: AsRef<str>,
    {
        self.decision(path)
            .map(|decision| decision.is_ignored())
            .unwrap_or(false)
    }

    /// Alias for [`Self::is_ignored`].
    pub fn matches<P>(&self, path: P) -> bool
    where
        P: AsRef<str>,
    {
        self.is_ignored(path)
    }
}

impl Default for IgnorePolicy {
    fn default() -> Self {
        Self {
            patterns: Vec::new(),
            ignore_git: DEFAULT_IGNORE_GIT,
        }
    }
}

/// Returns whether a path contains a `.git` component at any depth.
///
/// Both slash styles are accepted so cleanup and reconciliation code can use
/// this protection independently of capture-selection policy. Names such as
/// `.gitignore` and `git` do not match.
pub fn is_git_path(path: &str) -> bool {
    path.replace('\\', "/")
        .split('/')
        .any(|component| component.eq_ignore_ascii_case(".git"))
}

fn normalize_pattern(value: &str) -> Result<String, IgnorePatternError> {
    if value.trim().is_empty() {
        return Err(IgnorePatternError::Empty);
    }
    if value.len() > MAX_IGNORE_RULE_LENGTH {
        return Err(IgnorePatternError::TooLong);
    }
    if value.contains('\0') {
        return Err(IgnorePatternError::Nul);
    }
    let normalized = value.replace('\\', "/");
    if normalized.starts_with('/') {
        return Err(IgnorePatternError::Absolute);
    }
    if normalized.ends_with('/') || normalized.contains("//") {
        return Err(IgnorePatternError::EmptyComponent);
    }
    for component in normalized.split('/') {
        if component.is_empty() {
            return Err(IgnorePatternError::EmptyComponent);
        }
        if component == "." || component == ".." {
            return Err(IgnorePatternError::Traversal);
        }
        if component.chars().any(|character| {
            character.is_control() || matches!(character, ':' | '"' | '<' | '>' | '|')
        }) {
            return Err(IgnorePatternError::InvalidCharacter);
        }
    }
    Ok(normalized)
}

fn normalize_relative_path(value: &str) -> Result<String, IgnorePathError> {
    if value.len() > MAX_TREE_PATH_BYTES {
        return Err(IgnorePathError::TooLong);
    }
    if value.is_empty() {
        return Ok(String::new());
    }
    let normalized = value.replace('\\', "/");
    if normalized.starts_with('/') {
        return Err(IgnorePathError::Absolute);
    }
    if normalized.ends_with('/') || normalized.contains("//") {
        return Err(IgnorePathError::EmptyComponent);
    }
    for component in normalized.split('/') {
        if component == "." || component == ".." {
            return Err(IgnorePathError::Traversal);
        }
        if component.chars().any(|character| {
            character.is_control() || matches!(character, ':' | '*' | '?' | '"' | '<' | '>' | '|')
        }) {
            return Err(IgnorePathError::InvalidCharacter);
        }
    }
    Ok(normalized)
}

fn recursive_closure(states: &mut [bool], patterns: &[String]) {
    loop {
        let mut changed = false;
        for index in 0..patterns.len() {
            if states[index] && patterns[index] == "**" && !states[index + 1] {
                states[index + 1] = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
}

fn component_matches(pattern: &str, value: &str) -> bool {
    let pattern = pattern.chars().collect::<Vec<_>>();
    let value = value.chars().collect::<Vec<_>>();
    let mut pattern_index = 0;
    let mut value_index = 0;
    let mut star = None;
    let mut star_value_index = 0;

    while value_index < value.len() {
        if pattern_index < pattern.len()
            && (pattern[pattern_index] == '?' || pattern[pattern_index] == value[value_index])
        {
            pattern_index += 1;
            value_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == '*' {
            star = Some(pattern_index);
            star_value_index = value_index;
            pattern_index += 1;
        } else if let Some(star_index) = star {
            pattern_index = star_index + 1;
            star_value_index += 1;
            value_index = star_value_index;
        } else {
            return false;
        }
    }

    while pattern_index < pattern.len() && pattern[pattern_index] == '*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

/// Compatibility alias for callers that use rule terminology.
pub type IgnoreRule = IgnorePattern;

/// Compatibility alias for callers that use rule-error terminology.
pub type IgnoreRuleError = IgnorePatternError;

/// Compatibility alias for callers that use reason terminology.
pub type IgnoreReason = IgnoreMatch;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_and_deduplicates_patterns_deterministically() {
        let policy = IgnorePolicy::new(["z", r"src\generated", "z", "src/generated"])
            .expect("patterns should be valid");
        assert_eq!(
            policy.pattern_strings().collect::<Vec<_>>(),
            ["src/generated", "z"]
        );
    }

    #[test]
    fn matches_name_and_path_patterns() {
        let cases = [
            ("node_modules", "packages/node_modules/lib.js", true),
            ("node_modules", "packages/node_modules.txt", false),
            ("*.tmp", "cache/file.tmp", true),
            ("*.tmp", "cache/file.tmp.bak", false),
            ("build/generated", "build/generated/file.rs", true),
            ("build/generated", "other/build/generated/file.rs", false),
            ("src/*.rs", "src/main.rs", true),
            ("src/*.rs", "src/nested/main.rs", false),
            ("**/*.tmp", "a/b/file.tmp", true),
            ("**/*.tmp", "a/b/file.rs", false),
            (r"src\generated", "src/generated/file.rs", true),
        ];
        for (pattern, path, expected) in cases {
            let policy = IgnorePolicy::new([pattern]).expect("pattern should be valid");
            assert_eq!(policy.is_ignored(path), expected, "{pattern} vs {path}");
        }
    }

    #[test]
    fn recursive_patterns_match_zero_or_more_components() {
        let policy = IgnorePolicy::new(["build/**", "**/generated/**", "a/**/b"])
            .expect("patterns should be valid");
        for path in [
            "build",
            "build/one/two.txt",
            "x/generated",
            "x/generated/one.txt",
            "a/b",
            "a/x/y/b/file.txt",
        ] {
            assert!(policy.is_ignored(path), "{path} should be ignored");
        }
        assert!(!policy.is_ignored("ab/b"));
    }

    #[test]
    fn git_rule_is_component_exact_and_can_be_disabled() {
        let policy = IgnorePolicy::new([] as [&str; 0]).expect("empty policy should be valid");
        assert!(policy.is_ignored("one/two/.git/HEAD"));
        assert!(policy.is_ignored(r"one\two\.GIT\HEAD"));
        assert!(policy.is_ignored("one/.git"));
        assert!(!policy.is_ignored("one/.gitignore"));
        assert!(!policy.is_ignored("one/git/HEAD"));

        let included = policy.with_no_ignore_git();
        assert!(!included.is_ignored("one/two/.git/HEAD"));
        assert!(
            IgnorePolicy::new([".git"])
                .expect("explicit Git pattern should be valid")
                .with_no_ignore_git()
                .is_ignored("one/two/.git/HEAD")
        );
    }

    #[test]
    fn decisions_contain_only_relative_paths_and_match_reason() {
        let policy = IgnorePolicy::new(["private/*.key"]).expect("pattern should be valid");
        let decision = policy
            .decision("private/token.key")
            .expect("relative path should be valid");
        assert_eq!(decision.path(), "private/token.key");
        assert!(decision.is_ignored());
        assert_eq!(
            decision.matched_pattern().map(IgnorePattern::as_str),
            Some("private/*.key")
        );
        assert!(!format!("{decision:?}").contains("/home/"));
    }

    #[test]
    fn rejects_invalid_patterns_and_paths() {
        for pattern in [
            "",
            " ",
            "/absolute",
            "a//b",
            "a/",
            "../outside",
            "a/./b",
            "a\0b",
        ] {
            assert!(
                IgnorePattern::new(pattern).is_err(),
                "{pattern:?} should fail"
            );
        }
        assert!(matches!(
            IgnorePolicy::new(["/absolute"]),
            Err(IgnorePatternError::Absolute)
        ));
        assert!(matches!(
            IgnorePolicy::new(["a/../b"]),
            Err(IgnorePatternError::Traversal)
        ));
        assert!(matches!(
            IgnorePolicy::new(["a:b"]),
            Err(IgnorePatternError::InvalidCharacter)
        ));
        assert!(matches!(
            IgnorePolicy::new(["a\0b"]),
            Err(IgnorePatternError::Nul)
        ));
        assert!(matches!(
            IgnorePolicy::new(std::iter::repeat_n("rule", MAX_IGNORE_RULES + 1)),
            Err(IgnorePatternError::TooManyRules)
        ));

        assert!(matches!(
            IgnorePolicy::default().decision("/absolute"),
            Err(IgnorePathError::Absolute)
        ));
        assert!(matches!(
            IgnorePolicy::default().decision("a/../b"),
            Err(IgnorePathError::Traversal)
        ));
    }
}
