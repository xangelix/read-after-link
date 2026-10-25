//! The wrapper as cargo runs it: exit statuses, chaining, refusals, and a real cargo build of a
//! proc macro.

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const SHIM: &str = env!("CARGO_BIN_EXE_read-after-link");

fn shim() -> Command {
    let mut command = Command::new(SHIM);
    command
        .env_remove("READ_AFTER_LINK_WRAPPER")
        .env_remove("READ_AFTER_LINK_LOG");
    command
}

fn run(command: &mut Command) -> Output {
    command.output().expect("read-after-link runs")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// A command line that exits with `code`: `sh -c` or `cmd /C`.
fn exiting(code: i32) -> [String; 3] {
    if cfg!(windows) {
        ["cmd".into(), "/C".into(), format!("exit {code}")]
    } else {
        ["sh".into(), "-c".into(), format!("exit {code}")]
    }
}

/// A new, empty directory for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = env::temp_dir().join(format!("read-after-link-cli-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("the scratch directory is created");
    dir
}

#[test]
fn it_says_what_it_is() {
    let version = run(shim().arg("--version"));
    assert!(version.status.success());
    assert_eq!(
        text(&version.stdout).trim(),
        format!("read-after-link {}", env!("CARGO_PKG_VERSION"))
    );
    let help = run(shim().arg("--help"));
    assert!(help.status.success());
    assert!(text(&help.stdout).contains("READ_AFTER_LINK_WRAPPER"));
    let nothing = run(&mut shim());
    assert_eq!(nothing.status.code(), Some(2));
    assert!(text(&nothing.stderr).contains("Usage"));
}

#[test]
fn the_compilers_exit_status_comes_back() {
    for code in [0, 1, 3, 101] {
        let output = run(shim().args(exiting(code)));
        assert_eq!(output.status.code(), Some(code));
    }
}

#[test]
fn the_compiler_runs_through_the_wrapper() {
    // The wrapper gets the compiler and its arguments, in order.
    let (wrapper, line): (&str, Vec<String>) = if cfg!(windows) {
        (
            "cmd",
            vec!["/C".into(), "echo".into(), "through".into(), "cmd".into()],
        )
    } else {
        (
            "env",
            vec![
                "sh".into(),
                "-c".into(),
                "echo through \"$@\"".into(),
                "sh".into(),
                "env".into(),
            ],
        )
    };
    let output = run(shim().env("READ_AFTER_LINK_WRAPPER", wrapper).args(&line));
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert_eq!(text(&output.stdout).trim(), format!("through {wrapper}"));
    let [program, flag, script] = exiting(7);
    let output = run(shim()
        .env("READ_AFTER_LINK_WRAPPER", wrapper)
        .args(if cfg!(windows) {
            vec![flag, script]
        } else {
            vec![program, flag, script]
        }));
    assert_eq!(output.status.code(), Some(7));
}

#[test]
fn a_wrapper_that_is_itself_is_refused() {
    let output = run(shim().env("READ_AFTER_LINK_WRAPPER", SHIM).args(exiting(0)));
    assert_eq!(output.status.code(), Some(2));
    assert!(text(&output.stderr).contains("itself"));

    // A second name elsewhere (as a copy installed in front of sccache for the cc crate) refuses
    // its own path; one that names the original runs it, and the original refuses itself.
    // On Unix a hard link (next to the build, on its file system), not a copy: a process another
    // test forks while the copy is still open for writing keeps it open until it execs, and running
    // the copy then fails with ETXTBSY (rust-lang/rust#114554). Windows, which has no fork, gets a
    // copy: it refuses to delete a link to an executable that other tests are running.
    #[cfg(unix)]
    let dir = {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("read-after-link-cli-copy-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("the scratch directory is created");
        dir
    };
    #[cfg(not(unix))]
    let dir = scratch("copy");
    let copy = dir.join(Path::new(SHIM).file_name().expect("the shim has a name"));
    #[cfg(unix)]
    fs::hard_link(SHIM, &copy).expect("the shim gets a second name");
    #[cfg(not(unix))]
    fs::copy(SHIM, &copy).expect("the shim is copied");
    let output = run(Command::new(&copy)
        .env("READ_AFTER_LINK_WRAPPER", &copy)
        .env_remove("READ_AFTER_LINK_LOG")
        .args(exiting(0)));
    assert_eq!(output.status.code(), Some(2));
    let output = run(Command::new(&copy)
        .env("READ_AFTER_LINK_WRAPPER", SHIM)
        .env_remove("READ_AFTER_LINK_LOG")
        .args(exiting(0)));
    assert_eq!(output.status.code(), Some(2), "{}", text(&output.stderr));
    fs::remove_dir_all(dir).expect("the scratch directory is removed");
}

#[test]
fn a_wrapper_it_cannot_find_is_refused() {
    let output = run(shim()
        .env("READ_AFTER_LINK_WRAPPER", "read-after-link-no-such-wrapper")
        .args(exiting(0)));
    assert_eq!(output.status.code(), Some(2));
    assert!(text(&output.stderr).contains("no program"));
}

#[test]
fn a_compiler_it_cannot_run_fails_the_compile() {
    let output = run(shim().arg("read-after-link-no-such-compiler"));
    assert_eq!(output.status.code(), Some(1));
    assert!(text(&output.stderr).contains("could not run"));
}

/// Builds a workspace of a proc macro and a binary that uses it, through the wrapper, and returns
/// the log's lines.
fn build_with_cargo(name: &str, wrapper: Option<&OsStr>) -> Vec<String> {
    let dir = scratch(name);
    let write = |path: &str, contents: &str| {
        let path = dir.join(path);
        fs::create_dir_all(path.parent().expect("a parent")).expect("a directory");
        fs::write(path, contents).expect("a file");
    };
    write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"pm\", \"app\"]\nresolver = \"2\"\n",
    );
    write(
        "pm/Cargo.toml",
        "[package]\nname = \"pm\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\nproc-macro = true\n",
    );
    write(
        "pm/src/lib.rs",
        "use proc_macro::TokenStream;\n\n#[proc_macro]\npub fn answer(_: TokenStream) -> TokenStream {\n    \"42\".parse().unwrap()\n}\n",
    );
    write(
        "app/Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\npm = { path = \"../pm\" }\n",
    );
    write(
        "app/src/main.rs",
        "fn main() {\n    assert_eq!(pm::answer!(), 42);\n}\n",
    );
    let log = dir.join("read-after-link.log");
    let mut cargo = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    cargo
        .args(["build", "--offline", "--quiet"])
        .current_dir(&dir)
        .env("RUSTC_WRAPPER", SHIM)
        .env("READ_AFTER_LINK_LOG", &log)
        .env("CARGO_TARGET_DIR", dir.join("target"))
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .env_remove("CARGO_BUILD_RUSTC_WRAPPER")
        .env_remove("CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER");
    match wrapper {
        Some(wrapper) => cargo.env("READ_AFTER_LINK_WRAPPER", wrapper),
        None => cargo.env_remove("READ_AFTER_LINK_WRAPPER"),
    };
    let output = run(&mut cargo);
    assert!(output.status.success(), "{}", text(&output.stderr));
    let app = dir
        .join("target/debug")
        .join(format!("app{}", env::consts::EXE_SUFFIX));
    assert!(run(&mut Command::new(app)).status.success());
    let lines = fs::read_to_string(&log)
        .expect("the log was written")
        .lines()
        .map(str::to_owned)
        .collect();
    fs::remove_dir_all(dir).expect("the scratch directory is removed");
    lines
}

fn assert_reads_the_proc_macro(lines: &[String]) {
    assert_eq!(lines.len(), 1, "{lines:?}");
    let fields: Vec<&str> = lines[0].split('\t').collect();
    let path = Path::new(fields.last().expect("a path"));
    let name = path
        .file_name()
        .and_then(OsStr::to_str)
        .expect("a file name");
    assert!(
        name.starts_with(&format!("{}pm-", env::consts::DLL_PREFIX))
            && name.ends_with(env::consts::DLL_SUFFIX),
        "{name}"
    );
    if cfg!(windows) {
        assert_eq!(fields[0], "read", "{lines:?}");
        assert!(fields[1].parse::<u64>().expect("a length") > 0);
    } else {
        assert_eq!(fields[0], "found", "{lines:?}");
    }
}

#[test]
fn cargo_builds_a_proc_macro_through_it() {
    assert_reads_the_proc_macro(&build_with_cargo("cargo", None));
}

#[cfg(unix)]
#[test]
fn cargo_builds_a_proc_macro_through_it_and_another_wrapper() {
    assert_reads_the_proc_macro(&build_with_cargo("chained", Some(OsStr::new("env"))));
}
