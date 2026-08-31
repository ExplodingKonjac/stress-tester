//! Orchestration: build programs, spawn workers, display progress, report results.

use anyhow::{Context, Result, bail};
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::Cli;
use crate::checker;
use crate::judge::{Judge, Limits, TestReport, Verdict};
use crate::lang::{self, BuildOptions, CompilerConfig};
use crate::report;

pub fn run(cli: &Cli) -> Result<i32> {
    validate(cli)?;

    let cache_dir = cli
        .cache_dir
        .clone()
        .unwrap_or_else(lang::default_cache_dir);
    std::fs::create_dir_all(&cache_dir)
        .with_context(|| format!("failed to create cache dir {}", cache_dir.display()))?;

    let compilers = CompilerConfig {
        cc: cli.cc.clone(),
        cxx: cli.cxx.clone(),
        rustc: cli.rustc.clone(),
        python: cli.python.clone(),
    };

    // Build all programs (serially, so the cache is race-free).
    let candidate = build_program(
        "candidate",
        &cli.candidate,
        cli.cand_flags.as_deref(),
        &cache_dir,
        &compilers,
    )?;
    let reference = build_program(
        "reference",
        &cli.reference,
        cli.ref_flags.as_deref(),
        &cache_dir,
        &compilers,
    )?;
    let generator = build_program(
        "generator",
        &cli.generator,
        cli.gen_flags.as_deref(),
        &cache_dir,
        &compilers,
    )?;
    let checker_prog = match &cli.checker {
        Some(p) => Some(build_program(
            "checker",
            p,
            cli.checker_flags.as_deref(),
            &cache_dir,
            &compilers,
        )?),
        None => None,
    };

    let checker_kind = match (&checker_prog, &cli.check) {
        (Some(p), _) => crate::judge::Checker::Custom(p.program.clone()),
        (None, Some(name)) => {
            let c = checker::builtin_by_name(name)
                .ok_or_else(|| anyhow::anyhow!("unknown builtin checker `{name}`"))?;
            crate::judge::Checker::Builtin(c)
        }
        (None, None) => crate::judge::Checker::Builtin(checker::builtin_by_name("wcmp").unwrap()),
    };

    let secs = |s: f64| Duration::from_secs_f64(s);
    let mb = |mb: u64| mb * 1024 * 1024;
    let judge = Arc::new(Judge {
        candidate: candidate.program.clone(),
        reference: reference.program.clone(),
        generator: generator.program.clone(),
        checker: checker_kind,
        candidate_limits: Limits {
            time: secs(cli.time_limit),
            memory: mb(cli.memory_limit),
        },
        reference_limits: Limits {
            time: secs(cli.ref_time_limit),
            memory: mb(cli.ref_memory_limit),
        },
        generator_limits: Limits {
            time: secs(cli.gen_time_limit),
            memory: mb(cli.gen_memory_limit),
        },
        checker_limits: Limits {
            time: secs(cli.checker_time_limit),
            memory: mb(cli.checker_memory_limit),
        },
        generator_args: split_args(cli.gen_args.as_deref()),
        checker_args: split_args(cli.checker_args.as_deref()),
    });

    report::print_banner(&report::Banner {
        candidate: &candidate.program.display,
        candidate_lang: candidate.language.name(),
        reference: &reference.program.display,
        reference_lang: reference.language.name(),
        generator: &generator.program.display,
        generator_lang: generator.language.name(),
        checker: &judge.checker.describe(),
        limits: judge.candidate_limits,
        jobs: cli.jobs,
        start_seed: cli.start_seed,
        max_tests: cli.max_tests,
    });

    // Worker scratch directories.
    let temp_root = std::env::temp_dir().join(format!(
        "stress-tester-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&temp_root)
        .with_context(|| format!("failed to create {}", temp_root.display()))?;
    let _temp_guard = TempDirGuard(temp_root.clone());

    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = Arc::clone(&stop);
        ctrlc::set_handler(move || stop.store(true, Ordering::SeqCst))
            .context("failed to install Ctrl-C handler")?;
    }

    let (tx, rx) = mpsc::channel::<Result<TestReport, String>>();
    let next_seed = AtomicU64::new(cli.start_seed);
    let start = Instant::now();

    let pb = if cli.verbose {
        None
    } else {
        Some(make_progress_bar(cli.max_tests))
    };

    let mut passed: u64 = 0;
    let mut failure: Option<TestReport> = None;
    let mut harness_error: Option<String> = None;
    let jobs = cli.jobs;

    thread::scope(|s| {
        let mut handles = Vec::new();
        for worker_id in 0..jobs {
            let tx = tx.clone();
            let judge = Arc::clone(&judge);
            let stop = Arc::clone(&stop);
            let next_seed = &next_seed;
            let max_tests = cli.max_tests;
            let workdir = temp_root.join(format!("w{worker_id}"));
            handles.push(s.spawn(move || {
                if let Err(e) = std::fs::create_dir_all(&workdir) {
                    let _ = tx.send(Err(format!("failed to create {}: {e}", workdir.display())));
                    return;
                }
                loop {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let seed = next_seed.fetch_add(1, Ordering::SeqCst);
                    if let Some(max) = max_tests {
                        if seed >= cli.start_seed + max {
                            break;
                        }
                    }
                    match judge.run_test(seed, &workdir) {
                        Ok(rep) => {
                            if rep.verdict != Verdict::Ac {
                                stop.store(true, Ordering::SeqCst);
                            }
                            if tx.send(Ok(rep)).is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            stop.store(true, Ordering::SeqCst);
                            let _ = tx.send(Err(format!("{e:#}")));
                            break;
                        }
                    }
                }
            }));
        }
        drop(tx);

        // Display loop (single writer: this thread).
        loop {
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(Ok(rep)) => {
                    if rep.verdict == Verdict::Ac {
                        passed += 1;
                        if let Some(pb) = &pb {
                            pb.inc(1);
                            pb.set_message(format!("{passed} passed"));
                        } else {
                            report::print_test_line(&rep);
                        }
                    } else {
                        if failure.is_none() {
                            failure = Some(rep);
                        }
                        stop.store(true, Ordering::SeqCst);
                    }
                }
                Ok(Err(e)) => {
                    if harness_error.is_none() {
                        harness_error = Some(e);
                    }
                    stop.store(true, Ordering::SeqCst);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        for h in handles {
            let _ = h.join();
        }
    });

    if let Some(pb) = &pb {
        pb.finish_and_clear();
    }

    let interrupted = stop.load(Ordering::SeqCst) && failure.is_none() && harness_error.is_none();

    if let Some(e) = harness_error {
        eprintln!("{} {e}", "harness error:".red().bold());
        return Ok(2);
    }

    if let Some(rep) = &failure {
        let artifact_dir = save_artifacts(rep)?;
        report::print_failure(rep, &judge.candidate_limits, artifact_dir.as_deref());
        return Ok(match rep.verdict {
            Verdict::Failed => 2,
            _ => 1,
        });
    }

    report::print_summary(passed, start.elapsed(), interrupted);
    Ok(if interrupted { 130 } else { 0 })
}

fn validate(cli: &Cli) -> Result<()> {
    if cli.checker.is_some() && cli.check.is_some() {
        bail!("--checker and --check are mutually exclusive");
    }
    if let Some(name) = &cli.check {
        if checker::builtin_by_name(name).is_none() {
            bail!(
                "unknown builtin checker `{name}` (available: {})",
                checker::BUILTIN_NAMES.join(", ")
            );
        }
    }
    if cli.checker.is_none() && (cli.checker_args.is_some() || cli.checker_flags.is_some()) {
        bail!("--checker-args/--checker-flags require -k/--checker");
    }
    if cli.jobs == 0 {
        bail!("--jobs must be at least 1");
    }
    if cli.time_limit <= 0.0
        || cli.gen_time_limit <= 0.0
        || cli.ref_time_limit <= 0.0
        || cli.checker_time_limit <= 0.0
    {
        bail!("time limits must be positive");
    }
    if cli.memory_limit == 0
        || cli.gen_memory_limit == 0
        || cli.ref_memory_limit == 0
        || cli.checker_memory_limit == 0
    {
        bail!("memory limits must be at least 1 MB");
    }
    if let Some(n) = cli.max_tests {
        if n == 0 {
            bail!("--max-tests must be at least 1");
        }
    }
    Ok(())
}

fn build_program(
    role: &str,
    source: &Path,
    flags: Option<&str>,
    cache_dir: &Path,
    compilers: &CompilerConfig,
) -> Result<lang::BuildResult> {
    let opts = BuildOptions {
        cache_dir,
        compilers,
        flags,
    };
    let result = lang::build(source, &opts)?;
    let note = if !result.language.compiled() {
        "interpreted".to_owned()
    } else if result.cached {
        "cached".to_owned()
    } else {
        "compiled".to_owned()
    };
    println!(
        "  {} {:<10} {:<24} ({}, {})",
        "✓".green(),
        role,
        result.program.display,
        result.language.name(),
        note
    );
    Ok(result)
}

fn split_args(s: Option<&str>) -> Vec<String> {
    s.map(|s| s.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_default()
}

fn make_progress_bar(max_tests: Option<u64>) -> ProgressBar {
    let pb = match max_tests {
        Some(n) => ProgressBar::new(n),
        None => ProgressBar::new_spinner(),
    };
    let template = match max_tests {
        Some(_) => "{spinner:.green} {pos}/{len} tests | {elapsed} | {per_sec} | {msg}",
        None => "{spinner:.green} {pos} tests | {elapsed} | {per_sec} | {msg}",
    };
    pb.set_style(
        ProgressStyle::with_template(template)
            .unwrap_or_else(|_| ProgressStyle::default_bar())
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    pb.set_message("0 passed");
    pb
}

/// Copy the failing test's files into ./stress-failure/test-<seed>/.
fn save_artifacts(rep: &TestReport) -> Result<Option<PathBuf>> {
    let dir = PathBuf::from("stress-failure").join(format!("test-{}", rep.seed));
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("{} could not save artifacts: {e}", "warning:".yellow());
        return Ok(None);
    }
    let copy_in = |from: &Path, to: &str| {
        let _ = std::fs::copy(from, dir.join(to));
    };
    copy_in(&rep.input, "input.txt");
    copy_in(&rep.candidate_output, "candidate-output.txt");
    copy_in(&rep.candidate_stderr, "candidate-stderr.txt");
    copy_in(&rep.reference_output, "reference-output.txt");

    let text = format!(
        "stress-tester failure report\n\
         seed: {}\n\
         verdict: {:?}{}\n\
         candidate time: {}\n\
         candidate memory: {}\n\
         message: {}\n",
        rep.seed,
        rep.verdict,
        rep.failed_aux
            .map(|p| format!(" ({} failed)", p.name()))
            .unwrap_or_default(),
        report::fmt_duration(rep.candidate_time),
        crate::judge::format_memory(rep.candidate_memory),
        rep.message,
    );
    let _ = std::fs::write(dir.join("report.txt"), text);
    Ok(Some(dir))
}

struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
