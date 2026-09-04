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
    Java,
    JavaScript,
    Go,
    /// An already-built executable, run as-is.
    Binary,
}

impl Language {
    pub fn name(self) -> &'static str {
        match self {
            Language::C => "C",
            Language::Cpp => "C++",
            Language::Python => "Python",
            Language::Rust => "Rust",
            Language::Java => "Java",
            Language::JavaScript => "JavaScript",
            Language::Go => "Go",
            Language::Binary => "binary",
        }
    }

    fn tag(self) -> &'static str {
        match self {
            Language::C => "c",
            Language::Cpp => "cpp",
            Language::Python => "python",
            Language::Rust => "rust",
            Language::Java => "java",
            Language::JavaScript => "js",
            Language::Go => "go",
            Language::Binary => "binary",
        }
    }

    fn default_flags(self) -> &'static str {
        match self {
            Language::C => "-O2 -std=c11",
            Language::Cpp => "-O2 -std=c++17",
            Language::Rust => "-O",
            Language::Python
            | Language::JavaScript
            | Language::Java
            | Language::Go
            | Language::Binary => "",
        }
    }

    /// Run straight from source through an interpreter: nothing to compile.
    fn interpreted(self) -> bool {
        matches!(self, Language::Python | Language::JavaScript)
    }

    /// Argument that makes the toolchain print its version. `go` has no
    /// `--version`; asking for one fails and would leave the cache key blank.
    fn version_args(self) -> &'static [&'static str] {
        match self {
            Language::Go => &["version"],
            _ => &["--version"],
        }
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
        Some("java") => Ok(Language::Java),
        Some("js") => Ok(Language::JavaScript),
        Some("go") => Ok(Language::Go),
        _ if is_executable(path) => Ok(Language::Binary),
        _ => bail!(
            "cannot detect language of {} (supported: .c .cpp .cc .cxx .py .rs \
             .java .js .go, or an already-executable file)",
            path.display()
        ),
    }
}

/// Is this file runnable as-is? The executable bit on Unix, which also picks up
/// `#!` scripts; a real executable image on Windows, where `.bat`/`.cmd` are
/// excluded because `CreateProcess` cannot launch them without `cmd.exe`.
#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(windows)]
fn is_executable(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    matches!(ext.as_deref(), Some("exe" | "com")) && path.is_file()
}

/// Compiler/interpreter selection (CLI overrides win over $CC/$CXX and PATH).
#[derive(Debug, Default, Clone)]
pub struct CompilerConfig {
    pub cc: Option<String>,
    pub cxx: Option<String>,
    pub rustc: Option<String>,
    pub python: Option<String>,
    pub javac: Option<String>,
    pub java: Option<String>,
    pub go: Option<String>,
    pub node: Option<String>,
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
        Language::Java => cfg
            .javac
            .clone()
            .or_else(|| std::env::var("JAVAC").ok())
            .or_else(|| find_any(&["javac"])),
        Language::JavaScript => cfg.node.clone().or_else(|| find_any(&["node", "nodejs"])),
        Language::Go => cfg.go.clone().or_else(|| find_any(&["go"])),
        // `build` returns before resolving a toolchain for these.
        Language::Binary => None,
    };
    found.ok_or_else(|| match lang {
        Language::C => anyhow!("no C compiler found; install one or pass --cc"),
        Language::Cpp => anyhow!("no C++ compiler found; install one or pass --cxx"),
        Language::Rust => anyhow!("rustc not found; install Rust or pass --rustc"),
        Language::Python => anyhow!("Python interpreter not found; pass --python"),
        Language::Java => anyhow!("javac not found; install a JDK or pass --javac"),
        Language::JavaScript => anyhow!("node not found; install Node.js or pass --node"),
        Language::Go => anyhow!("go not found; install Go or pass --go"),
        Language::Binary => anyhow!("internal error: a binary needs no toolchain"),
    })
}

/// The `java` launcher, needed to *run* what `javac` produced.
fn resolve_java_launcher(cfg: &CompilerConfig) -> Result<String> {
    cfg.java
        .clone()
        .or_else(|| find_any(&["java"]))
        .ok_or_else(|| anyhow!("java not found; install a JRE or pass --java"))
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
    /// Per-program flags replacing the language defaults. Ignored for
    /// interpreted languages and for an already-built binary.
    pub flags: Option<&'a str>,
}

pub struct BuildResult {
    pub program: Program,
    pub language: Language,
    pub cached: bool,
}

impl BuildResult {
    /// How this program came to be, for the build banner.
    pub fn note(&self) -> &'static str {
        match self.language {
            Language::Binary => "prebuilt",
            l if l.interpreted() => "interpreted",
            _ if self.cached => "cached",
            _ => "compiled",
        }
    }
}

/// The command that builds a program into `dir`, and the argv that runs the
/// result afterwards.
fn recipe(
    lang: Language,
    tool: &str,
    cfg: &CompilerConfig,
    flags: &str,
    source: &Path,
    dir: &Path,
) -> Result<(Vec<OsString>, Vec<OsString>)> {
    let exe = dir.join(if cfg!(windows) { "prog.exe" } else { "prog" });
    let flags = flags.split_whitespace().map(OsString::from);
    let mut compile = vec![OsString::from(tool)];

    Ok(match lang {
        // javac writes a directory of .class files; the launcher needs the
        // class name, which Java requires to match the file stem.
        Language::Java => {
            let launcher = resolve_java_launcher(cfg)?;
            let class = source.file_stem().ok_or_else(|| {
                anyhow!(
                    "{} has no file stem to use as a class name",
                    source.display()
                )
            })?;
            compile.extend(flags);
            compile.push("-d".into());
            compile.push(dir.as_os_str().to_os_string());
            compile.push(source.as_os_str().to_os_string());
            let run = vec![
                OsString::from(launcher),
                "-cp".into(),
                dir.as_os_str().to_os_string(),
                class.to_os_string(),
            ];
            (compile, run)
        }
        // `go build` takes -o as a build flag, before the source.
        Language::Go => {
            compile.push("build".into());
            compile.push("-o".into());
            compile.push(exe.clone().into_os_string());
            compile.extend(flags);
            compile.push(source.as_os_str().to_os_string());
            (compile, vec![exe.into_os_string()])
        }
        Language::C | Language::Cpp | Language::Rust => {
            compile.extend(flags);
            compile.push(source.as_os_str().to_os_string());
            compile.push("-o".into());
            compile.push(exe.clone().into_os_string());
            (compile, vec![exe.into_os_string()])
        }
        Language::Python | Language::JavaScript | Language::Binary => {
            bail!("internal error: {} has no compile step", lang.name())
        }
    })
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
    let done = |argv: Vec<OsString>, cached: bool| -> Result<BuildResult> {
        Ok(BuildResult {
            program: Program {
                argv,
                display: display.clone(),
            },
            language: lang,
            cached,
        })
    };

    // Already runnable. argv[0] has to be a *path*: execvp searches $PATH for
    // anything without a separator, so a bare `a.out` would miss the local file.
    if lang == Language::Binary {
        let abs = std::path::absolute(source)
            .with_context(|| format!("failed to resolve {}", source.display()))?;
        return done(vec![abs.into_os_string()], false);
    }

    if lang.interpreted() {
        let interpreter = resolve_compiler(lang, opts.compilers)?;
        return done(
            vec![
                OsString::from(interpreter),
                source.as_os_str().to_os_string(),
            ],
            false,
        );
    }

    let compiler = resolve_compiler(lang, opts.compilers)?;
    let version = compiler_identity(&compiler, lang.version_args());
    let flags = opts
        .flags
        .map(str::to_owned)
        .unwrap_or_else(|| lang.default_flags().to_owned());

    let source_bytes =
        std::fs::read(source).with_context(|| format!("failed to read {}", source.display()))?;
    let hash = cache_hash(lang, &compiler, &version, &flags, &source_bytes);
    let dir = opts.cache_dir.join(&hash);
    let (compile, run) = recipe(lang, &compiler, opts.compilers, &flags, source, &dir)?;

    // ponytail: a marker file rather than an atomic rename, because Java's
    // artifact is a whole directory of .class files. Two processes compiling the
    // same hash at once can interleave writes; add a lock file if that bites.
    let built = dir.join(".built");
    if built.is_file() {
        return done(run, true);
    }

    std::fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let out_log = dir.join("build.stdout");
    let err_log = dir.join("build.stderr");
    let spec = RunSpec {
        argv: &compile,
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
    std::fs::write(&built, []).context("failed to mark the cached build complete")?;

    done(run, false)
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

fn compiler_identity(compiler: &str, version_args: &[&str]) -> String {
    let out = std::process::Command::new(compiler)
        .args(version_args)
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
            ("a.java", Language::Java),
            ("a.js", Language::JavaScript),
            ("a.go", Language::Go),
        ] {
            assert_eq!(detect_language(Path::new(name)).unwrap(), lang);
        }
        // Unknown extension on a file that is not executable (does not exist).
        assert!(detect_language(Path::new("a.zzz")).is_err());
        assert!(detect_language(Path::new("noextension")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn detect_executable_without_extension() {
        use std::os::unix::fs::PermissionsExt;
        let path =
            std::env::temp_dir().join(format!("stress-tester-detect-{}", std::process::id()));
        std::fs::write(&path, b"#!/bin/sh\nexit 0\n").unwrap();

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(detect_language(&path).is_err());

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(detect_language(&path).unwrap(), Language::Binary);

        let _ = std::fs::remove_file(&path);
    }

    /// The Java and Go invocations are not exercised by the test machine's
    /// toolchain, so pin their argv shape here. Unix-only: the assertions spell
    /// out path separators and the bare `prog` name.
    #[cfg(unix)]
    #[test]
    fn recipes_are_shaped_per_language() {
        let cfg = CompilerConfig {
            java: Some("my-java".to_owned()),
            ..Default::default()
        };
        let dir = Path::new("/cache/h");
        let strs = |v: Vec<OsString>| -> Vec<String> {
            v.into_iter()
                .map(|s| s.to_string_lossy().into_owned())
                .collect()
        };

        let (compile, run) = recipe(
            Language::Java,
            "javac",
            &cfg,
            "-g",
            Path::new("Main.java"),
            dir,
        )
        .unwrap();
        assert_eq!(
            strs(compile),
            ["javac", "-g", "-d", "/cache/h", "Main.java"]
        );
        assert_eq!(strs(run), ["my-java", "-cp", "/cache/h", "Main"]);

        // -o is a `go build` flag, so it precedes the source file.
        let (compile, run) =
            recipe(Language::Go, "go", &cfg, "", Path::new("sol.go"), dir).unwrap();
        assert_eq!(
            strs(compile),
            ["go", "build", "-o", "/cache/h/prog", "sol.go"]
        );
        assert_eq!(strs(run), ["/cache/h/prog"]);

        let (compile, _) =
            recipe(Language::Cpp, "g++", &cfg, "-O2", Path::new("s.cpp"), dir).unwrap();
        assert_eq!(
            strs(compile),
            ["g++", "-O2", "s.cpp", "-o", "/cache/h/prog"]
        );

        assert!(
            recipe(
                Language::Python,
                "python3",
                &cfg,
                "",
                Path::new("s.py"),
                dir
            )
            .is_err()
        );
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
