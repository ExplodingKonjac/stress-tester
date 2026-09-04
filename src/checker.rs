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

/// testlib's `compress`: elide the middle of a long value so a runaway line
/// cannot flood the terminal and `checker.log`.
fn compress(s: &str) -> String {
    let n = s.chars().count();
    if n <= 64 {
        return s.to_owned();
    }
    let head: String = s.chars().take(30).collect();
    let tail: String = s.chars().skip(n - 31).collect();
    format!("{head}...{tail}")
}

/// testlib's `__testlib_isInfinite`: a magnitude past 1e300, not IEEE infinity.
/// NaN is deliberately not "infinite" here, matching testlib.
fn testlib_infinite(x: f64) -> bool {
    x.abs() > 1e300
}

/// testlib's `doubleCompare`: accept within an absolute *or* relative error.
fn double_compare(expected: f64, result: f64, eps: f64) -> bool {
    let eps = eps + 1e-15;
    if expected.is_nan() {
        return result.is_nan();
    }
    if testlib_infinite(expected) {
        return testlib_infinite(result) && (result > 0.0) == (expected > 0.0);
    }
    if result.is_nan() || testlib_infinite(result) {
        return false;
    }
    if (result - expected).abs() <= eps {
        return true;
    }
    let (lo, hi) = (expected * (1.0 - eps), expected * (1.0 + eps));
    result >= lo.min(hi) && result <= lo.max(hi)
}

/// testlib's `doubleDelta`: the smaller of the absolute and relative error.
fn double_delta(expected: f64, result: f64) -> f64 {
    let absolute = (result - expected).abs();
    if expected.abs() > 1e-9 {
        absolute.min((absolute / expected).abs())
    } else {
        absolute
    }
}

/// Parse a real the way testlib's `readDouble` does: only `0-9 . e E + -` are
/// accepted, so `nan` and `inf` are malformed rather than values to compare.
fn parse_double(t: &str) -> Option<f64> {
    if !t
        .bytes()
        .all(|c| c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E' | b'+' | b'-'))
    {
        return None;
    }
    t.parse::<f64>().ok()
}

/// Parse an integer the way testlib's `readLong` does: 64-bit, an optional
/// leading `-`, and no `+`, no redundant leading zeros, no `-0`.
fn parse_long(t: &str) -> Option<i64> {
    let digits = t.strip_prefix('-').unwrap_or(t);
    if digits.is_empty() || !digits.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if digits.len() > 1 && digits.starts_with('0') {
        return None;
    }
    if digits == "0" && t.starts_with('-') {
        return None;
    }
    t.parse::<i64>().ok()
}

/// Token-wise exact comparison (whitespace-insensitive).
fn wcmp(output: &str, answer: &str) -> Option<String> {
    let a = tokens(output);
    let b = tokens(answer);
    match a.iter().zip(&b).position(|(x, y)| x != y) {
        Some(i) => Some(format!(
            "token {} differs: expected {:?}, found {:?}",
            i + 1,
            compress(b[i]),
            compress(a[i])
        )),
        None if a.len() != b.len() => {
            Some(format!("expected {} token(s), found {}", b.len(), a.len()))
        }
        None => None,
    }
}

/// Line-wise comparison. Each line is compared as a *sequence of tokens*
/// (testlib's `lcmp`), so whitespace inside a line is not significant;
/// trailing blank lines are ignored.
fn lcmp(output: &str, answer: &str) -> Option<String> {
    fn norm(s: &str) -> Vec<&str> {
        let mut v: Vec<&str> = s.lines().collect();
        while v.last().is_some_and(|l| l.trim().is_empty()) {
            v.pop();
        }
        v
    }
    let a = norm(output);
    let b = norm(answer);
    match a.iter().zip(&b).position(|(x, y)| tokens(x) != tokens(y)) {
        Some(i) => Some(format!(
            "line {} differs: expected {:?}, found {:?}",
            i + 1,
            compress(b[i].trim_end()),
            compress(a[i].trim_end())
        )),
        None if a.len() != b.len() => {
            Some(format!("expected {} line(s), found {}", b.len(), a.len()))
        }
        None => None,
    }
}

/// Integer-sequence comparison (64-bit, testlib's strict integer format).
fn ncmp(output: &str, answer: &str) -> Option<String> {
    let parse = |s: &str, who: &str| -> Result<Vec<i64>, String> {
        tokens(s)
            .iter()
            .enumerate()
            .map(|(i, t)| {
                parse_long(t).ok_or_else(|| {
                    format!("{who} token {} is not an integer: {:?}", i + 1, compress(t))
                })
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
    match a.iter().zip(&b).position(|(x, y)| x != y) {
        Some(i) => Some(format!(
            "token {} differs: expected {}, found {}",
            i + 1,
            b[i],
            a[i]
        )),
        None if a.len() != b.len() => {
            Some(format!("expected {} token(s), found {}", b.len(), a.len()))
        }
        None => None,
    }
}

/// Real-number comparison with testlib's absolute-or-relative tolerance.
fn rcmp(output: &str, answer: &str, eps: f64) -> Option<String> {
    let parse = |s: &str, who: &str| -> Result<Vec<f64>, String> {
        tokens(s)
            .iter()
            .enumerate()
            .map(|(i, t)| {
                parse_double(t).ok_or_else(|| {
                    format!("{who} token {} is not a number: {:?}", i + 1, compress(t))
                })
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
        if !double_compare(*y, *x, eps) {
            return Some(format!(
                "token {} differs: expected {y}, found {x} (error {:.3e} > {eps:.3e})",
                i + 1,
                double_delta(*y, *x),
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
                "invalid answer (bug in reference?): token {} is not YES/NO: {:?}",
                i + 1,
                compress(t)
            ));
        }
    }
    for (i, t) in a.iter().enumerate() {
        if t != "YES" && t != "NO" {
            return Some(format!("token {} is not YES/NO: {:?}", i + 1, compress(t)));
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
        // testlib compares each line token-wise: inner spacing is not significant
        assert_eq!(lcmp("a  b", "a b"), None);
        // ...but the token sequence itself still has to match
        assert!(lcmp("ab", "a b").is_some());
        assert!(lcmp("a\nb", "a\nc").is_some());
        // trailing blank lines ignored
        assert_eq!(lcmp("a\n\n\n", "a\n"), None);
        assert!(lcmp("a\nb", "a").is_some());
    }

    #[test]
    fn ncmp_basic() {
        assert_eq!(ncmp("5 -3\n", "5 -3"), None);
        assert_eq!(ncmp("0", "0"), None);
        // order matters, like testlib
        assert!(ncmp("5 -3", "-3 5").is_some());
        // testlib's readLong rejects `+`, leading zeros and `-0`
        assert!(ncmp("+5", "5").is_some());
        assert!(ncmp("05", "5").is_some());
        assert!(ncmp("-0", "0").is_some());
        assert!(ncmp("5", "5.0").is_some());
        assert!(ncmp("1 2", "1 2 3").is_some());
        // 64-bit range, like testlib's `long long`
        assert_eq!(ncmp("9223372036854775807", "9223372036854775807"), None);
        assert!(ncmp("9223372036854775808", "1").is_some());
    }

    #[test]
    fn rcmp_absolute_or_relative() {
        let eps = 1e-4;
        assert_eq!(rcmp("3.14159\n", "3.14158", eps), None);
        assert!(rcmp("3.15", "3.14", eps).is_some());
        assert!(rcmp("1", "1\n2", eps).is_some());
        // relative tolerance rescues large magnitudes, where the absolute
        // error is far past eps (testlib's doubleCompare)
        assert_eq!(rcmp("1000000000.5", "1000000000.0", eps), None);
        assert!(rcmp("1000000000.0", "2000000000.0", eps).is_some());
        // around zero only the absolute error applies
        assert_eq!(rcmp("0.00001", "0", eps), None);
        assert!(rcmp("0.001", "0", eps).is_some());
        // nan/inf are malformed reals, never equal to anything
        assert!(rcmp("nan", "5", eps).is_some());
        assert!(rcmp("nan", "nan", eps).is_some());
        assert!(rcmp("inf", "5", eps).is_some());
        // ...but testlib treats a *magnitude* past 1e300 as infinite, and two
        // same-signed infinities compare equal
        assert_eq!(rcmp("1e400", "1e400", eps), None);
        assert!(rcmp("-1e400", "1e400", eps).is_some());
        assert!(rcmp("1e400", "5", eps).is_some());
    }

    #[test]
    fn compress_long_values() {
        assert_eq!(compress("short"), "short");
        let long = "x".repeat(200);
        let out = compress(&long);
        assert_eq!(out.chars().count(), 64);
        assert!(out.contains("..."));
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
