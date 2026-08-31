//! Per-test judging pipeline: generate -> run candidate -> run reference -> check.

use crate::checker::{BuiltinChecker, CustomOutcome, interpret_custom_exit, read_for_check};
use crate::lang::Program;
use crate::runner::{self, ExitKind, RunSpec};
use anyhow::Result;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Ac,
    Wa,
    Tle,
    Mle,
    Re,
    /// Generator / reference / checker failed: not the candidate's fault.
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuxProgram {
    Generator,
    Reference,
    Checker,
}

impl AuxProgram {
    pub fn name(self) -> &'static str {
        match self {
            AuxProgram::Generator => "generator",
            AuxProgram::Reference => "reference",
            AuxProgram::Checker => "checker",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub time: Duration,
    /// Bytes.
    pub memory: u64,
}

pub enum Checker {
    Builtin(BuiltinChecker),
    Custom(Program),
}

impl Checker {
    pub fn describe(&self) -> String {
        match self {
            Checker::Builtin(c) => format!("{} (builtin)", c.name()),
            Checker::Custom(p) => format!("{} (custom)", p.display),
        }
    }
}

pub struct Judge {
    pub candidate: Program,
    pub reference: Program,
    pub generator: Program,
    pub checker: Checker,
    pub candidate_limits: Limits,
    pub reference_limits: Limits,
    pub generator_limits: Limits,
    pub checker_limits: Limits,
    pub generator_args: Vec<String>,
    pub checker_args: Vec<String>,
}

pub struct TestReport {
    pub seed: u64,
    pub verdict: Verdict,
    pub candidate_time: Duration,
    pub candidate_memory: u64,
    pub message: String,
    pub failed_aux: Option<AuxProgram>,
    pub input: PathBuf,
    pub candidate_output: PathBuf,
    pub candidate_stderr: PathBuf,
    pub reference_output: PathBuf,
}

impl TestReport {
    fn base(seed: u64, workdir: &Path) -> Self {
        TestReport {
            seed,
            verdict: Verdict::Ac,
            candidate_time: Duration::ZERO,
            candidate_memory: 0,
            message: String::new(),
            failed_aux: None,
            input: workdir.join("input.txt"),
            candidate_output: workdir.join("candidate-output.txt"),
            candidate_stderr: workdir.join("candidate-stderr.txt"),
            reference_output: workdir.join("reference-output.txt"),
        }
    }
}

impl Judge {
    pub fn run_test(&self, seed: u64, workdir: &Path) -> Result<TestReport> {
        let mut report = TestReport::base(seed, workdir);
        let gen_err = workdir.join("generator-stderr.txt");
        let ref_err = workdir.join("reference-stderr.txt");
        let chk_out = workdir.join("checker-stdout.txt");
        let chk_err = workdir.join("checker-stderr.txt");

        // 1. Generator: argv = <gen> <extra args...> <seed>
        let mut argv = self.generator.argv.clone();
        argv.extend(self.generator_args.iter().map(OsString::from));
        argv.push(seed.to_string().into());
        let stats = run(
            &argv,
            None,
            Some(&report.input),
            Some(&gen_err),
            self.generator_limits,
        )?;
        if let Some(msg) = aux_failure(
            AuxProgram::Generator,
            &stats,
            &gen_err,
            self.generator_limits,
        ) {
            fail_aux(&mut report, AuxProgram::Generator, msg);
            return Ok(report);
        }

        // 2. Candidate.
        let stats = run(
            &self.candidate.argv,
            Some(&report.input),
            Some(&report.candidate_output),
            Some(&report.candidate_stderr),
            self.candidate_limits,
        )?;
        report.candidate_time = stats.wall_time;
        report.candidate_memory = stats.peak_memory;
        if stats.timed_out {
            report.verdict = Verdict::Tle;
            report.message = format!(
                "time limit exceeded: {:.3}s > {:.3}s",
                stats.wall_time.as_secs_f64(),
                self.candidate_limits.time.as_secs_f64()
            );
            return Ok(report);
        }
        if stats.memory_exceeded || stats.peak_memory > self.candidate_limits.memory {
            report.verdict = Verdict::Mle;
            report.message = format!(
                "memory limit exceeded: {} > {}",
                format_memory(stats.peak_memory),
                format_memory(self.candidate_limits.memory)
            );
            return Ok(report);
        }
        if stats.exit != ExitKind::Code(0) {
            report.verdict = Verdict::Re;
            report.message = format!(
                "{}{}",
                match stats.exit {
                    ExitKind::Code(c) => format!("exited with code {c}"),
                    ExitKind::Signal(s) => format!("killed by {}", signal_name(s)),
                },
                stderr_tail(&report.candidate_stderr)
            );
            return Ok(report);
        }

        // 3. Reference.
        let stats = run(
            &self.reference.argv,
            Some(&report.input),
            Some(&report.reference_output),
            Some(&ref_err),
            self.reference_limits,
        )?;
        if let Some(msg) = aux_failure(
            AuxProgram::Reference,
            &stats,
            &ref_err,
            self.reference_limits,
        ) {
            fail_aux(&mut report, AuxProgram::Reference, msg);
            return Ok(report);
        }

        // 4. Check.
        match &self.checker {
            Checker::Builtin(c) => {
                let output = read_for_check(&report.candidate_output);
                let answer = read_for_check(&report.reference_output);
                if let Some(msg) = c.check(&output, &answer) {
                    report.verdict = Verdict::Wa;
                    report.message = msg;
                }
            }
            Checker::Custom(prog) => {
                let mut argv = prog.argv.clone();
                argv.extend([
                    report.input.clone().into_os_string(),
                    report.candidate_output.clone().into_os_string(),
                    report.reference_output.clone().into_os_string(),
                ]);
                argv.extend(self.checker_args.iter().map(OsString::from));
                let stats = run(
                    &argv,
                    None,
                    Some(&chk_out),
                    Some(&chk_err),
                    self.checker_limits,
                )?;
                if stats.timed_out {
                    fail_aux(
                        &mut report,
                        AuxProgram::Checker,
                        format!(
                            "checker timed out: {:.3}s > {:.3}s",
                            stats.wall_time.as_secs_f64(),
                            self.checker_limits.time.as_secs_f64()
                        ),
                    );
                    return Ok(report);
                }
                if stats.memory_exceeded || stats.peak_memory > self.checker_limits.memory {
                    fail_aux(
                        &mut report,
                        AuxProgram::Checker,
                        format!(
                            "checker exceeded memory limit: {} > {}",
                            format_memory(stats.peak_memory),
                            format_memory(self.checker_limits.memory)
                        ),
                    );
                    return Ok(report);
                }
                match stats.exit {
                    ExitKind::Code(c) => match interpret_custom_exit(c) {
                        CustomOutcome::Ac => {}
                        CustomOutcome::Wa => {
                            report.verdict = Verdict::Wa;
                            let msg = std::fs::read_to_string(&chk_err).unwrap_or_default();
                            let msg: String = msg.trim().chars().take(400).collect();
                            report.message = if msg.is_empty() {
                                "wrong answer".to_owned()
                            } else {
                                format!("wrong answer: {msg}")
                            };
                        }
                        CustomOutcome::Failed => {
                            fail_aux(
                                &mut report,
                                AuxProgram::Checker,
                                format!("checker exited with code {c}{}", stderr_tail(&chk_err)),
                            );
                        }
                    },
                    ExitKind::Signal(s) => {
                        fail_aux(
                            &mut report,
                            AuxProgram::Checker,
                            format!("checker killed by {}", signal_name(s)),
                        );
                    }
                }
            }
        }

        Ok(report)
    }
}

fn run(
    argv: &[OsString],
    stdin_file: Option<&Path>,
    stdout_file: Option<&Path>,
    stderr_file: Option<&Path>,
    limits: Limits,
) -> Result<runner::RunStats> {
    let spec = RunSpec {
        argv,
        stdin_file,
        stdout_file,
        stderr_file,
        time_limit: limits.time,
        memory_limit: limits.memory,
    };
    runner::run(&spec).map_err(Into::into)
}

/// Mark a test as FAILED because an auxiliary (trusted) program misbehaved.
fn fail_aux(report: &mut TestReport, which: AuxProgram, msg: String) {
    report.verdict = Verdict::Failed;
    report.failed_aux = Some(which);
    report.message = msg;
}

/// Failure message if an auxiliary (trusted) program misbehaved.
fn aux_failure(
    which: AuxProgram,
    stats: &runner::RunStats,
    stderr_file: &Path,
    limits: Limits,
) -> Option<String> {
    if stats.timed_out {
        return Some(format!(
            "{} timed out: {:.3}s > {:.3}s",
            which.name(),
            stats.wall_time.as_secs_f64(),
            limits.time.as_secs_f64()
        ));
    }
    if stats.memory_exceeded || stats.peak_memory > limits.memory {
        return Some(format!(
            "{} exceeded memory limit: {} > {}",
            which.name(),
            format_memory(stats.peak_memory),
            format_memory(limits.memory)
        ));
    }
    match stats.exit {
        ExitKind::Code(0) => None,
        ExitKind::Code(c) => Some(format!(
            "{} exited with code {c}{}",
            which.name(),
            stderr_tail(stderr_file)
        )),
        ExitKind::Signal(s) => Some(format!(
            "{} killed by {}{}",
            which.name(),
            signal_name(s),
            stderr_tail(stderr_file)
        )),
    }
}

fn stderr_tail(path: &Path) -> String {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    let trimmed = content.trim_end();
    if trimmed.is_empty() {
        return String::new();
    }
    let lines: Vec<&str> = trimmed.lines().collect();
    let start = lines.len().saturating_sub(4);
    format!("\n  stderr: {}", lines[start..].join(" | "))
}

pub fn format_memory(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    let mb = bytes as f64 / MB;
    if mb >= 1.0 {
        format!("{mb:.1} MB")
    } else {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    }
}

#[cfg(unix)]
fn signal_name(sig: i32) -> String {
    match sig {
        4 => "SIGILL".to_owned(),
        6 => "SIGABRT".to_owned(),
        7 => "SIGBUS".to_owned(),
        8 => "SIGFPE".to_owned(),
        9 => "SIGKILL".to_owned(),
        11 => "SIGSEGV".to_owned(),
        15 => "SIGTERM".to_owned(),
        _ => format!("signal {sig}"),
    }
}

#[cfg(windows)]
fn signal_name(_sig: i32) -> String {
    String::new()
}
