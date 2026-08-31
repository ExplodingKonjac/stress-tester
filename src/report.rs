//! Terminal output: banner, progress, per-test lines, failure reports, summary.

use colored::ColoredString;
use colored::Colorize;
use std::path::Path;
use std::time::Duration;

use crate::judge::{Limits, TestReport, Verdict};

pub fn verdict_badge(v: Verdict) -> ColoredString {
    let s = match v {
        Verdict::Ac => "AC",
        Verdict::Wa => "WA",
        Verdict::Tle => "TLE",
        Verdict::Mle => "MLE",
        Verdict::Re => "RE",
        Verdict::Failed => "FAILED",
    };
    match v {
        Verdict::Ac => s.green().bold(),
        Verdict::Wa | Verdict::Re | Verdict::Failed => s.red().bold(),
        Verdict::Tle => s.yellow().bold(),
        Verdict::Mle => s.magenta().bold(),
    }
}

pub fn fmt_duration(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s < 1.0 {
        format!("{} ms", d.as_millis())
    } else {
        format!("{s:.2} s")
    }
}

pub struct Banner<'a> {
    pub candidate: &'a str,
    pub candidate_lang: &'a str,
    pub reference: &'a str,
    pub reference_lang: &'a str,
    pub generator: &'a str,
    pub generator_lang: &'a str,
    pub checker: &'a str,
    pub limits: Limits,
    pub jobs: usize,
    pub start_seed: u64,
    pub max_tests: Option<u64>,
}

pub fn print_banner(b: &Banner) {
    println!("{}", "Test Information".bold());
    println!(
        "  {:<10} {} ({})",
        "candidate:", b.candidate, b.candidate_lang
    );
    println!(
        "  {:<10} {} ({})",
        "reference:", b.reference, b.reference_lang
    );
    println!(
        "  {:<10} {} ({})",
        "generator:", b.generator, b.generator_lang
    );
    println!("  {:<10} {}", "checker:", b.checker);
    println!(
        "  {:<10} TL {} / ML {}",
        "limits:",
        format_secs(b.limits.time),
        crate::judge::format_memory(b.limits.memory),
    );
    println!(
        "  {:<10} {} jobs, {} tests, initial seed = {}",
        "metadata:",
        b.jobs,
        match b.max_tests {
            Some(n) => n.to_string(),
            None => "∞".to_owned(),
        },
        b.start_seed,
    );
    println!();
}

fn format_secs(d: Duration) -> String {
    format!("{}s", d.as_secs_f64())
}

/// Streaming line for one finished test, e.g. `AC on test #42  (3 ms / 5.1 MB)`.
pub fn print_test_line(report: &TestReport) {
    println!(
        "{} on test #{:<6} {:>10}  {:>10}",
        verdict_badge(report.verdict),
        report.seed,
        fmt_duration(report.candidate_time),
        crate::judge::format_memory(report.candidate_memory),
    );
}

pub fn print_failure(report: &TestReport, limits: &Limits, artifact_dir: Option<&Path>) {
    println!();
    let title = match report.failed_aux {
        Some(p) => format!("{} ({})", verdict_badge(report.verdict), p.name()),
        None => format!("{}", verdict_badge(report.verdict)),
    };
    println!("{} on test #{}", title, report.seed);
    println!(
        "         time: {} (limit {})   memory: {} (limit {})",
        fmt_duration(report.candidate_time),
        format_secs(limits.time),
        crate::judge::format_memory(report.candidate_memory),
        crate::judge::format_memory(limits.memory),
    );
    if !report.message.is_empty() {
        println!("         {}", report.message);
    }
    if report.verdict == Verdict::Wa
        && let Some(diff) = render_diff(&report.reference_output, &report.candidate_output)
    {
        println!(
            "         diff ({} = expected, {} = found):",
            "-".green(),
            "+".red()
        );
        for line in diff {
            println!("{line}");
        }
    }
    if let Some(dir) = artifact_dir {
        println!("         saved: {}", dir.display());
    }
}

const MAX_DIFF_LINES: usize = 30;

/// Colored unified-ish diff between reference (expected) and candidate output.
pub fn render_diff(expected: &Path, found: &Path) -> Option<Vec<String>> {
    let read = |p: &Path| -> Option<String> {
        let meta = std::fs::metadata(p).ok()?;
        if meta.len() > 1_000_000 {
            return None;
        }
        std::fs::read(p)
            .ok()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
    };
    let a = read(expected)?;
    let b = read(found)?;
    if a == b {
        return None;
    }

    use similar::{ChangeTag, TextDiff};
    let diff = TextDiff::from_lines(&a, &b);
    let mut out = Vec::new();
    for change in diff.iter_all_changes() {
        if out.len() >= MAX_DIFF_LINES {
            out.push(format!("         {} more lines...", "...".dimmed()));
            break;
        }
        // Trim before coloring: `ColoredString` derefs to the *uncolored*
        // input, so trimming after `.green()`/`.red()` would drop the color.
        let text = format!("{change}").trim_end_matches('\n').to_owned();
        let (prefix, colored) = match change.tag() {
            ChangeTag::Equal => (' ', text.normal()),
            ChangeTag::Delete => ('-', text.green()),
            ChangeTag::Insert => ('+', text.red()),
        };
        out.push(format!("         {prefix} {colored}"));
    }
    Some(out)
}

pub fn print_summary(passed: u64, elapsed: Duration, interrupted: bool) {
    println!();
    let secs = elapsed.as_secs_f64();
    let rate = if secs > 0.0 {
        passed as f64 / secs
    } else {
        0.0
    };
    if interrupted {
        println!(
            "{} interrupted after {passed} test(s), all passed so far ({secs:.1}s)",
            "STOP".yellow().bold()
        );
    } else {
        println!(
            "{} {passed} test(s) passed in {secs:.1}s ({rate:.1} tests/s)",
            "OK".green().bold()
        );
    }
}
