//! Checkers: Rust reimplementations of the default testlib checkers,
//! plus exit-code interpretation for custom testlib-format checkers.

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinChecker {
    Wcmp,
    Lcmp,
    Ncmp,
    Rcmp4,
    Rcmp6,
    Rcmp9,
    Nyesno,
}

pub const BUILTIN_NAMES: &[&str] = &["wcmp", "lcmp", "ncmp", "rcmp4", "rcmp6", "rcmp9", "nyesno"];

pub fn builtin_by_name(name: &str) -> Option<BuiltinChecker> {
    match name {
        "wcmp" => Some(BuiltinChecker::Wcmp),
        "lcmp" => Some(BuiltinChecker::Lcmp),
        "ncmp" => Some(BuiltinChecker::Ncmp),
        "rcmp4" => Some(BuiltinChecker::Rcmp4),
        "rcmp6" => Some(BuiltinChecker::Rcmp6),
        "rcmp9" => Some(BuiltinChecker::Rcmp9),
        "nyesno" => Some(BuiltinChecker::Nyesno),
        _ => None,
    }
}

impl BuiltinChecker {
    pub fn name(self) -> &'static str {
        match self {
            BuiltinChecker::Wcmp => "wcmp",
            BuiltinChecker::Lcmp => "lcmp",
            BuiltinChecker::Ncmp => "ncmp",
            BuiltinChecker::Rcmp4 => "rcmp4",
            BuiltinChecker::Rcmp6 => "rcmp6",
            BuiltinChecker::Rcmp9 => "rcmp9",
            BuiltinChecker::Nyesno => "nyesno",
        }
    }

    /// Compare candidate output against reference answer.
    /// `None` means AC, `Some(message)` means WA.
    pub fn check(self, output: &str, answer: &str) -> Option<String> {
        match self {
            BuiltinChecker::Wcmp => wcmp(output, answer),
            BuiltinChecker::Lcmp => lcmp(output, answer),
            BuiltinChecker::Ncmp => ncmp(output, answer),
            BuiltinChecker::Rcmp4 => rcmp(output, answer, 1e-4),
            BuiltinChecker::Rcmp6 => rcmp(output, answer, 1e-6),
            BuiltinChecker::Rcmp9 => rcmp(output, answer, 1e-9),
            BuiltinChecker::Nyesno => nyesno(output, answer),
        }
    }
}

/// Read a file as lossy UTF-8 for checking.
pub fn read_for_check(path: &Path) -> String {
    std::fs::read(path)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}

fn tokens(s: &str) -> Vec<&str> {
    s.split_whitespace().collect()
}

/// Token-wise exact comparison (whitespace-insensitive).
fn wcmp(output: &str, answer: &str) -> Option<String> {
    let a = tokens(output);
    let b = tokens(answer);
    if a == b {
        return None;
    }
    let pos = a.iter().zip(&b).position(|(x, y)| x != y);
    match pos {
        Some(i) => Some(format!(
            "token {i} differs: expected {:?}, found {:?}",
            b[i], a[i]
        )),
        None => Some(format!("expected {} token(s), found {}", b.len(), a.len())),
    }
}

/// Line-wise comparison, ignoring trailing whitespace and trailing blank lines.
fn lcmp(output: &str, answer: &str) -> Option<String> {
    fn norm(s: &str) -> Vec<&str> {
        let mut v: Vec<&str> = s.lines().map(|l| l.trim_end()).collect();
        while v.last() == Some(&"") {
            v.pop();
        }
        v
    }
    let a = norm(output);
    let b = norm(answer);
    if a == b {
        return None;
    }
    let pos = a.iter().zip(&b).position(|(x, y)| x != y);
    match pos {
        Some(i) => Some(format!(
            "line {i} differs: expected {:?}, found {:?}",
            b[i], a[i]
        )),
        None => Some(format!("expected {} line(s), found {}", b.len(), a.len())),
    }
}

/// Integer-sequence comparison.
fn ncmp(output: &str, answer: &str) -> Option<String> {
    let parse = |s: &str, who: &str| -> Result<Vec<i128>, String> {
        tokens(s)
            .iter()
            .enumerate()
            .map(|(i, t)| {
                t.parse::<i128>()
                    .map_err(|_| format!("{who} token {i} is not an integer: {t:?}"))
            })
            .collect()
    };
    let a = match parse(output, "output") {
        Ok(v) => v,
        Err(e) => return Some(e),
    };
    let b = match parse(answer, "answer") {
        Ok(v) => v,
        Err(e) => return Some(format!("invalid answer (bug in reference?): {e}")),
    };
    if a == b {
        return None;
    }
    let pos = a.iter().zip(&b).position(|(x, y)| x != y);
    match pos {
        Some(i) => Some(format!(
            "token {i} differs: expected {}, found {}",
            b[i], a[i]
        )),
        None => Some(format!("expected {} token(s), found {}", b.len(), a.len())),
    }
}

/// Real-number comparison with tolerance.
fn rcmp(output: &str, answer: &str, eps: f64) -> Option<String> {
    let parse = |s: &str, who: &str| -> Result<Vec<f64>, String> {
        tokens(s)
            .iter()
            .enumerate()
            .map(|(i, t)| {
                t.parse::<f64>()
                    .map_err(|_| format!("{who} token {i} is not a number: {t:?}"))
            })
            .collect()
    };
    let a = match parse(output, "output") {
        Ok(v) => v,
        Err(e) => return Some(e),
    };
    let b = match parse(answer, "answer") {
        Ok(v) => v,
        Err(e) => return Some(format!("invalid answer (bug in reference?): {e}")),
    };
    if a.len() != b.len() {
        return Some(format!("expected {} token(s), found {}", b.len(), a.len()));
    }
    for (i, (x, y)) in a.iter().zip(&b).enumerate() {
        if (x - y).abs() > eps {
            return Some(format!(
                "token {i} differs: expected {y}, found {x} (|diff| > {eps})"
            ));
        }
    }
    None
}

/// Sequence of case-insensitive YES/NO tokens, compared pairwise
/// (testlib's `nyesno`). Extra tokens on either side are rejected;
/// empty sequences match.
fn nyesno(output: &str, answer: &str) -> Option<String> {
    let toks = |s: &str| -> Vec<String> {
        s.split_whitespace()
            .map(|t| t.to_ascii_uppercase())
            .collect()
    };
    let a = toks(output);
    let b = toks(answer);

    for (i, t) in b.iter().enumerate() {
        if t != "YES" && t != "NO" {
            return Some(format!(
                "invalid answer (bug in reference?): token {} is not YES/NO: {t:?}",
                i + 1
            ));
        }
    }
    for (i, t) in a.iter().enumerate() {
        if t != "YES" && t != "NO" {
            return Some(format!("token {} is not YES/NO: {t:?}", i + 1));
        }
    }
    for (i, (x, y)) in a.iter().zip(&b).enumerate() {
        if x != y {
            return Some(format!("token {} differs: expected {y}, found {x}", i + 1));
        }
    }
    if a.len() != b.len() {
        return Some(format!("expected {} token(s), found {}", b.len(), a.len()));
    }
    None
}

/// Outcome of a custom testlib checker's exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustomOutcome {
    Ac,
    Wa,
    Failed,
}

/// testlib exit codes: 0 = OK, 1 = _wa, 2 = _pe, 3+ = _fail/_dirt/_points...
pub fn interpret_custom_exit(code: i32) -> CustomOutcome {
    match code {
        0 => CustomOutcome::Ac,
        1 | 2 => CustomOutcome::Wa,
        _ => CustomOutcome::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wcmp_basic() {
        assert_eq!(wcmp("1 2 3\n", "  1\n2\t3\n"), None);
        assert!(wcmp("1 2 3", "1 2 4").is_some());
        assert!(wcmp("1 2", "1 2 3").is_some());
        // CRLF handled
        assert_eq!(wcmp("a\r\nb\r\n", "a b"), None);
    }

    #[test]
    fn lcmp_basic() {
        assert_eq!(lcmp("hi\r\n", "hi\n"), None);
        assert_eq!(lcmp("hi  \n", "hi\n"), None);
        // inner spacing matters
        assert!(lcmp("a  b", "a b").is_some());
        // trailing blank lines ignored
        assert_eq!(lcmp("a\n\n\n", "a\n"), None);
    }

    #[test]
    fn ncmp_basic() {
        assert_eq!(ncmp("5 -3\n", "5 -3"), None);
        // order matters, like testlib
        assert!(ncmp("5 -3", "-3 5").is_some());
        // numeric equality despite different formatting
        assert_eq!(ncmp("+5", "5"), None);
        assert!(ncmp("5", "5.0").is_some());
        assert!(ncmp("1 2", "1 2 3").is_some());
    }

    #[test]
    fn rcmp_tolerance() {
        let eps = 1e-4;
        assert_eq!(rcmp("3.14159\n", "3.14158", eps), None);
        assert!(rcmp("3.15", "3.14", eps).is_some());
        assert!(rcmp("1", "1\n2", eps).is_some());
    }

    #[test]
    fn nyesno_basic() {
        // Single token, case-insensitive.
        assert_eq!(nyesno("YES\n", "yes"), None);
        assert_eq!(nyesno("No", "no"), None);
        // Sequence of tokens.
        assert_eq!(nyesno("yes no YES", "YES NO yes"), None);
        // Mismatch in a later token.
        assert!(nyesno("yes no", "yes yes").is_some());
        // Length mismatch either way.
        assert!(nyesno("yes", "yes no").is_some());
        assert!(nyesno("yes no", "yes").is_some());
        // Invalid output token.
        assert!(nyesno("maybe", "yes").is_some());
        // Empty sequences match (testlib: "Empty output").
        assert_eq!(nyesno("", " \n"), None);
    }

    #[test]
    fn custom_exit_codes() {
        assert_eq!(interpret_custom_exit(0), CustomOutcome::Ac);
        assert_eq!(interpret_custom_exit(1), CustomOutcome::Wa);
        assert_eq!(interpret_custom_exit(2), CustomOutcome::Wa);
        assert_eq!(interpret_custom_exit(3), CustomOutcome::Failed);
    }
}
