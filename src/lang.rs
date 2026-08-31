//! Language detection, compilation, and the compile cache.
//!
//! The cache key is the SHA-256 of (language, compiler identity + version,
//! compiler flags, source content), so any change recompiles that one program.

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::runner::{self, ExitKind, RunSpec};

const COMPILE_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    C,
    Cpp,
    Python,
    Rust,
}

impl Language {
    pub fn name(self) -> &'static str {
        match self {
            Language::C => "C",
            Language::Cpp => "C++",
            Language::Python => "Python",
            Language::Rust => "Rust",
        }
    }

    fn tag(self) -> &'static str {
        match self {
            Language::C => "c",
            Language::Cpp => "cpp",
            Language::Python => "python",
            Language::Rust => "rust",
        }
    }

    fn default_flags(self) -> &'static str {
        match self {
            Language::C => "-O2 -std=c11",
            Language::Cpp => "-O2 -std=c++17",
            Language::Python => "",
            Language::Rust => "-O",
        }
    }

    pub fn compiled(self) -> bool {
        !matches!(self, Language::Python)
    }
}

pub fn detect_language(path: &Path) -> Result<Language> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("c") => Ok(Language::C),
        Some("cpp" | "cc" | "cxx" | "c++") => Ok(Language::Cpp),
        Some("py") => Ok(Language::Python),
        Some("rs") => Ok(Language::Rust),
        _ => bail!(
            "cannot detect language of {} (supported: .c .cpp .cc .cxx .py .rs)",
            path.display()
        ),
    }
}

/// Compiler/interpreter selection (CLI overrides win over $CC/$CXX and PATH).
#[derive(Debug, Default, Clone)]
pub struct CompilerConfig {
    pub cc: Option<String>,
    pub cxx: Option<String>,
    pub rustc: Option<String>,
    pub python: Option<String>,
}

fn find_any(names: &[&str]) -> Option<String> {
    names
        .iter()
        .find(|n| which::which(n).is_ok())
        .map(|n| n.to_string())
}

fn resolve_compiler(lang: Language, cfg: &CompilerConfig) -> Result<String> {
    let found = match lang {
        Language::C => cfg
            .cc
            .clone()
            .or_else(|| std::env::var("CC").ok())
            .or_else(|| find_any(&["cc", "gcc", "clang"])),
        Language::Cpp => cfg
            .cxx
            .clone()
            .or_else(|| std::env::var("CXX").ok())
            .or_else(|| find_any(&["c++", "g++", "clang++"])),
        Language::Rust => cfg.rustc.clone().or_else(|| find_any(&["rustc"])),
        Language::Python => cfg
            .python
            .clone()
            .or_else(|| find_any(&["python3", "python"])),
    };
    found.ok_or_else(|| match lang {
        Language::C => anyhow!("no C compiler found; install one or pass --cc"),
        Language::Cpp => anyhow!("no C++ compiler found; install one or pass --cxx"),
        Language::Rust => anyhow!("rustc not found; install Rust or pass --rustc"),
        Language::Python => anyhow!("Python interpreter not found; pass --python"),
    })
}

/// A runnable program: full argv (interpreter + script, or just the binary).
#[derive(Debug, Clone)]
pub struct Program {
    pub argv: Vec<OsString>,
    pub display: String,
}

pub struct BuildOptions<'a> {
    pub cache_dir: &'a Path,
    pub compilers: &'a CompilerConfig,
    /// Per-program flags replacing the language defaults (ignored for Python).
    pub flags: Option<&'a str>,
}

pub struct BuildResult {
    pub program: Program,
    pub language: Language,
    pub cached: bool,
}

pub fn build(source: &Path, opts: &BuildOptions) -> Result<BuildResult> {
    if !source.exists() {
        bail!("file not found: {}", source.display());
    }
    let lang = detect_language(source)?;
    let display = source
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();

    if lang == Language::Python {
        let python = resolve_compiler(lang, opts.compilers)?;
        return Ok(BuildResult {
            program: Program {
                argv: vec![OsString::from(python), source.as_os_str().to_os_string()],
                display,
            },
            language: lang,
            cached: false,
        });
    }

    let compiler = resolve_compiler(lang, opts.compilers)?;
    let version = compiler_identity(&compiler);
    let flags = opts
        .flags
        .map(str::to_owned)
        .unwrap_or_else(|| lang.default_flags().to_owned());

    let source_bytes =
        std::fs::read(source).with_context(|| format!("failed to read {}", source.display()))?;
    let hash = cache_hash(lang, &compiler, &version, &flags, &source_bytes);

    let dir = opts.cache_dir.join(&hash);
    let exe = dir.join(if cfg!(windows) { "prog.exe" } else { "prog" });
    if exe.is_file() {
        return Ok(BuildResult {
            program: Program {
                argv: vec![exe.into_os_string()],
                display,
            },
            language: lang,
            cached: true,
        });
    }

    std::fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let part = dir.join("prog.part");
    // Leftover from a previously interrupted compile; rename must not collide.
    let _ = std::fs::remove_file(&part);

    let mut argv: Vec<OsString> = vec![compiler.clone().into()];
    argv.extend(flags.split_whitespace().map(OsString::from));
    argv.push(source.as_os_str().to_os_string());
    argv.push("-o".into());
    argv.push(part.clone().into_os_string());

    let out_log = dir.join("build.stdout");
    let err_log = dir.join("build.stderr");
    let spec = RunSpec {
        argv: &argv,
        stdin_file: None,
        stdout_file: Some(&out_log),
        stderr_file: Some(&err_log),
        time_limit: COMPILE_TIMEOUT,
        memory_limit: u64::MAX,
    };
    let stats =
        runner::run(&spec).with_context(|| format!("failed to run compiler `{compiler}`"))?;

    if stats.timed_out {
        let _ = std::fs::remove_dir_all(&dir);
        bail!("compilation of {display} timed out");
    }
    if stats.exit != ExitKind::Code(0) {
        let log = std::fs::read_to_string(&err_log).unwrap_or_default();
        let _ = std::fs::remove_dir_all(&dir);
        bail!(
            "compilation of {display} failed ({}):\n{}",
            match stats.exit {
                ExitKind::Code(c) => format!("exit code {c}"),
                ExitKind::Signal(s) => format!("killed by signal {s}"),
            },
            truncate_head(&log, 4000)
        );
    }
    let _ = std::fs::remove_file(&out_log);
    let _ = std::fs::remove_file(&err_log);

    std::fs::rename(&part, &exe).with_context(|| "failed to finalize cached binary")?;

    Ok(BuildResult {
        program: Program {
            argv: vec![exe.into_os_string()],
            display,
        },
        language: lang,
        cached: false,
    })
}

fn cache_hash(lang: Language, compiler: &str, version: &str, flags: &str, source: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(lang.tag().as_bytes());
    h.update([0]);
    h.update(compiler.as_bytes());
    h.update([0]);
    h.update(version.as_bytes());
    h.update([0]);
    h.update(flags.as_bytes());
    h.update([0]);
    h.update(source);
    format!("{:x}", h.finalize())
}

fn compiler_identity(compiler: &str) -> String {
    let out = std::process::Command::new(compiler)
        .arg("--version")
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout);
            text.lines().next().unwrap_or("").trim().to_owned()
        }
        _ => String::new(),
    }
}

fn truncate_head(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.trim_end().to_owned();
    }
    let mut cut = max;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n...", &s[..cut])
}

/// Default cache directory: <system cache>/stress-tester, or a temp fallback.
pub fn default_cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("stress-tester")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_by_extension() {
        for (name, lang) in [
            ("a.c", Language::C),
            ("a.CPP", Language::Cpp),
            ("a.cc", Language::Cpp),
            ("a.py", Language::Python),
            ("a.rs", Language::Rust),
        ] {
            assert_eq!(detect_language(Path::new(name)).unwrap(), lang);
        }
        assert!(detect_language(Path::new("a.java")).is_err());
    }

    #[test]
    fn hash_depends_on_all_inputs() {
        let a = cache_hash(Language::Cpp, "g++", "15.2", "-O2", b"src");
        assert_ne!(a, cache_hash(Language::Cpp, "g++", "15.3", "-O2", b"src"));
        assert_ne!(a, cache_hash(Language::Cpp, "g++", "15.2", "-O1", b"src"));
        assert_ne!(a, cache_hash(Language::Cpp, "g++", "15.2", "-O2", b"src2"));
        assert_ne!(a, cache_hash(Language::Rust, "g++", "15.2", "-O2", b"src"));
        assert_eq!(a, cache_hash(Language::Cpp, "g++", "15.2", "-O2", b"src"));
    }
}
