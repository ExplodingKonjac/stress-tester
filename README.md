# stress-tester

Stress-test competitive-programming solutions against a trusted reference implementation,
using a testlib-style generator. Written in Rust, runs on Linux, macOS and Windows.

Each test is one cycle: run the generator with a seed, feed its output to both the
candidate and the reference, then compare the two outputs with a checker. The first
failing test stops the run and its data is saved to disk.

```
$ stress-tester -c wa.cpp -r ac.cpp -g gen.cpp -n 100 -j 4
Building programs
  ✓ candidate  wa.cpp                   (C++, compiled)
  ✓ reference  ac.cpp                   (C++, cached)
  ✓ generator  gen.cpp                  (C++, cached)
Test Information
  candidate: wa.cpp (C++)
  reference: ac.cpp (C++)
  generator: gen.cpp (C++)
  checker:   wcmp (builtin)
  limits:    TL 1s / ML 512.0 MB
  metadata:  4 jobs, 100 tests, initial seed = 1

AC on test #1            3 ms      2.1 MB
AC on test #2            3 ms      2.1 MB
WA on test #3            3 ms      2.1 MB

WA on test #3
         cpu: 3 ms (limit 1s)   wall: 4 ms   memory: 2.1 MB (limit 512.0 MB)
         token 1 differs: expected "20", found "18"
         diff (- = expected, + = found):
         - 20
         + 18
         saved: stress-output
```

## Install

You need a Rust toolchain to build the tool. If you don't have one, install it from
[rustup.rs](https://rustup.rs):

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

On Windows, download and run `rustup-init.exe` from the same page instead.

Then build and install `stress-tester`:

```sh
git clone https://github.com/ExplodingKonjac/stress-tester
cd stress-tester
cargo install --path .
```

This puts the `stress-tester` binary in `~/.cargo/bin` (`%USERPROFILE%\.cargo\bin` on
Windows), which rustup already adds to your `PATH`. Check that it works:

```sh
stress-tester --version
```

If the command is not found, open a new shell so the updated `PATH` takes effect.

You also need a compiler or interpreter for the *programs you want to test* — a C/C++
compiler (`gcc`/`clang`), `rustc`, a JDK, `go`, `node`, or `python3`, depending on what
your solutions are written in. Any of them already on your `PATH` is picked up
automatically.

## Usage

```sh
stress-tester -c <candidate> -r <reference> -g <generator> [options]
```

The three programs are usually given as **source files**; the tool detects the language
from the extension, compiles what needs compiling, and caches the binaries.

| Extension | Language | Default flags | Toolchain |
|---|---|---|---|
| `.c` | C | `-O2 -std=c11` | `--cc`, `$CC`, then `cc`/`gcc`/`clang` |
| `.cpp` `.cc` `.cxx` `.c++` | C++ | `-O2 -std=c++17` | `--cxx`, `$CXX`, then `c++`/`g++`/`clang++` |
| `.rs` | Rust | `-O` | `--rustc`, then `rustc` |
| `.go` | Go | — | `--go`, then `go` |
| `.java` | Java | — | `--javac`, `$JAVAC`, then `javac`; run with `--java`/`java` |
| `.py` | Python | — (interpreted) | `--python`, then `python3`/`python` |
| `.js` | JavaScript | — (interpreted) | `--node`, then `node`/`nodejs` |

For Java the class name has to match the file stem, as the language requires:
`Main.java` is compiled with `javac -d <cache>` and run as `java -cp <cache> Main`.
Compiler flags reach `javac`, not the launcher, so JVM options such as `-Xss` cannot be
set. PyPy needs no special support — pass `--python pypy3`.

### Pre-built programs

Any of the four programs may instead be an **already-built executable**, which is run
as-is with no compilation and no caching:

```sh
stress-tester -c ./my_solution -r ./trusted -g gen.cpp
```

A file is treated as pre-built when its extension is not one of the above and it is
executable: the executable bit on Linux/macOS (which also covers `#!` scripts), or an
`.exe`/`.com` extension on Windows. `.bat`/`.cmd` are not accepted, since Windows cannot
launch them without going through `cmd.exe`.

The generator receives the seed as its **last** argument, so any testlib generator using
`registerGen(argc, argv, 1)` works unchanged. Seeds start at `--start-seed` and increase
by one per test, which makes a run reproducible: re-running with the same seed regenerates
the same failing test.

### Common options

```
-c, --candidate <FILE>   program under test
-r, --reference <FILE>   trusted program
-g, --generator <FILE>   testcase generator (gets the seed as last argument)
-n, --max-tests <N>      stop after N tests (default: run until failure or Ctrl-C)
-j, --jobs <N>           parallel workers (default 1)
-t, --time-limit <SEC>   candidate CPU time limit (default 1.0)
-m, --memory-limit <MB>  candidate memory limit (default 512)
-s, --start-seed <N>     first seed (default 1)
-o, --output <DIR>       where to save the failing test (default stress-output)
    --check <NAME>       builtin checker
-k, --checker <FILE>     custom testlib-format checker
    --gen-args "<ARGS>"  extra generator arguments, inserted before the seed
```

Time limits are **CPU time** (user + system), not wall clock, so a verdict does not
change because the machine is busy — raising `-j` is safe. See
[Limits](#how-it-works) for the wall-clock backstop that still catches hangs.

Auxiliary programs have their own generous limits (`--gen-time-limit`,
`--ref-memory-limit`, `--checker-time-limit`, …; 60 s / 512 MB by default), and each
program can override its compiler flags (`--cand-flags`, `--ref-flags`, `--gen-flags`,
`--checker-flags`). Run `stress-tester --help` for the full list.

### Checkers

By default outputs are compared token-wise (`wcmp`). `--check <name>` picks another
builtin, all reimplementations of the corresponding testlib checker:

| Name | Comparison |
|---|---|
| `wcmp` | tokens, exact (whitespace-insensitive) — default |
| `lcmp` | lines, each compared as a sequence of tokens (so spacing *inside* a line is insignificant), ignoring trailing blank lines |
| `ncmp` | sequence of 64-bit integers, in testlib's strict format: no `+`, no leading zeros, no `-0` |
| `rcmp4` / `rcmp6` / `rcmp9` | sequence of reals, max absolute **or** relative error 1e-4 / 1e-6 / 1e-9; `nan`/`inf` in the output are rejected |
| `nyesno` | sequence of case-insensitive `YES`/`NO` tokens |

These follow testlib's comparison semantics, with one deliberate exception: a
length mismatch is always reported, in either direction. testlib's `lcmp` and
`rcmp*` stop at the end of the answer file and silently ignore trailing output,
which would hide a real bug during stress testing.

For problems with multiple valid answers, pass your own checker with `-k`. It is invoked
in testlib order — `checker <input> <output> <answer>` (plus `--checker-args`) — and its
exit code is read the testlib way: `0` = accepted, `1`/`2` = wrong answer, anything else
means the checker itself failed. Its stdout and stderr become `checker.log`.

`--check` and `-k` are mutually exclusive.

### Verdicts and exit codes

`AC`, `WA`, `TLE`, `MLE`, `RE`, and `FAILED` — the last one meaning the generator,
reference, or checker misbehaved, so the candidate is not at fault.

| Exit code | Meaning |
|---|---|
| `0` | all tests passed |
| `1` | candidate failed a test (`WA`/`TLE`/`MLE`/`RE`) |
| `2` | harness error, or `FAILED` |
| `130` | interrupted with Ctrl-C, nothing had failed yet |

### Failing test artifacts

When a test fails, `--output` (default `stress-output/`) receives:

```
data.in       generator output
data.out      candidate output
data.ans      reference output
checker.log   checker verdict message
```

Successful tests leave nothing behind, and the scratch directory is removed on exit.

## Development

```sh
cargo test          # unit tests for checkers, language detection, cache keys, limits
cargo clippy
```

Test fixtures live in `tests/fixtures/` (`gen.cpp`, `ac.cpp`, `wa.cpp`).
