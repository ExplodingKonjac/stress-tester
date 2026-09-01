mod app;
mod checker;
mod judge;
mod lang;
mod report;
mod runner;

use clap::Parser;
use colored::Colorize;

/// Stress-test competitive-programming solutions against a reference
/// implementation using a testlib-style generator.
#[derive(Parser, Debug)]
#[command(name = "stress-tester", version, about, verbatim_doc_comment)]
struct Cli {
    /// Candidate program to test
    #[arg(short, long)]
    candidate: std::path::PathBuf,

    /// Reference (trusted) program
    #[arg(short, long)]
    reference: std::path::PathBuf,

    /// Testcase generator (testlib-style, receives the seed as last argument)
    #[arg(short, long)]
    generator: std::path::PathBuf,

    /// Custom checker program (testlib format: <input> <output> <answer>)
    #[arg(short = 'k', long)]
    checker: Option<std::path::PathBuf>,

    /// Builtin checker name: wcmp, lcmp, ncmp, rcmp4, rcmp6, rcmp9, nyesno
    #[arg(long)]
    check: Option<String>,

    /// Time limit for the candidate, in seconds
    #[arg(short = 't', long, default_value_t = 1.0)]
    time_limit: f64,

    /// Memory limit for the candidate, in MB
    #[arg(short = 'm', long, default_value_t = 512)]
    memory_limit: u64,

    /// Maximum number of testcases (default: infinite)
    #[arg(short = 'n', long)]
    max_tests: Option<u64>,

    /// Directory to save the failing test's data (data.in, data.out, data.ans, checker.log)
    #[arg(short = 'o', long, default_value = "stress-output")]
    output: std::path::PathBuf,

    /// Number of parallel judging workers
    #[arg(short = 'j', long, default_value_t = 1)]
    jobs: usize,

    /// Seed passed to the first testcase (increases by 1 per test)
    #[arg(short = 's', long, default_value_t = 1)]
    start_seed: u64,

    /// Time limit for the generator, in seconds
    #[arg(long, default_value_t = 60.0)]
    gen_time_limit: f64,

    /// Memory limit for the generator, in MB
    #[arg(long, default_value_t = 512)]
    gen_memory_limit: u64,

    /// Time limit for the reference, in seconds
    #[arg(long, default_value_t = 60.0)]
    ref_time_limit: f64,

    /// Memory limit for the reference, in MB
    #[arg(long, default_value_t = 512)]
    ref_memory_limit: u64,

    /// Time limit for a custom checker, in seconds
    #[arg(long, default_value_t = 60.0)]
    checker_time_limit: f64,

    /// Memory limit for a custom checker, in MB
    #[arg(long, default_value_t = 512)]
    checker_memory_limit: u64,

    /// Extra command-line arguments for the generator (before the seed)
    #[arg(long, allow_hyphen_values = true)]
    gen_args: Option<String>,

    /// Extra command-line arguments for a custom checker (after the three files)
    #[arg(long, allow_hyphen_values = true)]
    checker_args: Option<String>,

    /// Compiler flags for the candidate program (replaces language defaults)
    #[arg(long, allow_hyphen_values = true)]
    cand_flags: Option<String>,

    /// Compiler flags for the reference program
    #[arg(long, allow_hyphen_values = true)]
    ref_flags: Option<String>,

    /// Compiler flags for the generator program
    #[arg(long, allow_hyphen_values = true)]
    gen_flags: Option<String>,

    /// Compiler flags for the custom checker program
    #[arg(long, allow_hyphen_values = true)]
    checker_flags: Option<String>,

    /// C compiler override
    #[arg(long)]
    cc: Option<String>,

    /// C++ compiler override
    #[arg(long)]
    cxx: Option<String>,

    /// Rust compiler (rustc) override
    #[arg(long)]
    rustc: Option<String>,

    /// Python interpreter override
    #[arg(long)]
    python: Option<String>,

    /// Compilation cache directory
    #[arg(long)]
    cache_dir: Option<std::path::PathBuf>,

    /// Print one line per test instead of a progress bar
    #[arg(long)]
    verbose: bool,
}

fn main() {
    let cli = Cli::parse();
    match app::run(&cli) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("{} {e:#}", "error:".red().bold());
            std::process::exit(2);
        }
    }
}
