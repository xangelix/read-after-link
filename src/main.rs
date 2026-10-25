//! `read-after-link`: a rustc wrapper (`RUSTC_WRAPPER`) that reads each dynamic library a compile
//! links once, start to end, before it exits, and so before cargo starts anything that loads it.
//!
//! On Windows, a DLL just written through a memory-mapped view (as the MSVC linker writes it) is
//! sometimes mapped with pages of zeros when its first cached read overlaps its first load as an
//! executable image, and it stays that way until the file is replaced. sccache's server reads
//! every `--extern` file while a probe compile loads the same proc macros, which is such an
//! overlap. A read that completes before anything loads the file prevents it. The README has the
//! details and the measurements.
//!
//! The wrapper runs the compiler it is given, through `READ_AFTER_LINK_WRAPPER` when that is set
//! (sccache, for example), and passes its exit status back. After a successful compile whose crate
//! types make a dynamic library (`proc-macro`, `dylib`, `cdylib`), it reads that library on
//! Windows. Elsewhere it reads nothing.

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{self, Command, ExitStatus};

/// Names another wrapper to run the compiler through (for example sccache).
const WRAPPER_VAR: &str = "READ_AFTER_LINK_WRAPPER";
/// Names a file to append one line to per library.
const LOG_VAR: &str = "READ_AFTER_LINK_LOG";
/// The read size: what sccache uses to hash files.
const CHUNK: usize = 128 * 1024;
/// The crate types whose output is a dynamic library.
const DYNAMIC: [&str; 3] = ["proc-macro", "dylib", "cdylib"];

fn usage() -> String {
    format!(
        "read-after-link {version}

A rustc wrapper. It runs the compiler it is given, through {WRAPPER_VAR} when that is set, and
when a compile links a dynamic library (proc-macro, dylib, cdylib), it reads the new library once,
start to end, before it exits. It reads only on Windows.

Usage: RUSTC_WRAPPER=read-after-link cargo build
       read-after-link <compiler> [<argument>...]

Environment:
  {WRAPPER_VAR}  another wrapper to run the compiler through (for example sccache)
  {LOG_VAR}      a file to append one line to per library: read, found, missing, or failed
",
        version = env!("CARGO_PKG_VERSION"),
    )
}

fn main() {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    if let [only] = args.as_slice() {
        match only.to_str() {
            Some("--version" | "-V") => {
                println!("read-after-link {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            Some("--help" | "-h") => {
                print!("{}", usage());
                return;
            }
            _ => {}
        }
    }
    let Some((compiler, compiler_args)) = args.split_first() else {
        eprint!("{}", usage());
        process::exit(2);
    };
    // Found before the compile runs: cargo removes an argument file once the compiler exits.
    let library = library(&parse(&expand(compiler_args)));
    let mut command = match command(compiler, compiler_args) {
        Ok(command) => command,
        Err(message) => {
            eprintln!("read-after-link: {message}");
            process::exit(2);
        }
    };
    let code = match command.status() {
        Ok(status) => exit_code(status),
        Err(error) => {
            eprintln!(
                "read-after-link: could not run {}: {error}",
                command.get_program().to_string_lossy()
            );
            1
        }
    };
    if code == 0 {
        if let Some(library) = library {
            settle(&library);
        }
    }
    process::exit(code);
}

/// The command that compiles: the compiler, or `READ_AFTER_LINK_WRAPPER` with the compiler as its
/// first argument.
fn command(compiler: &OsStr, args: &[OsString]) -> Result<Command, String> {
    let Some(wrapper) = env::var_os(WRAPPER_VAR).filter(|wrapper| !wrapper.is_empty()) else {
        let mut command = Command::new(compiler);
        command.args(args);
        return Ok(command);
    };
    let Some(path) = locate(&wrapper) else {
        return Err(format!(
            "{WRAPPER_VAR} names no program it can find: {}",
            wrapper.to_string_lossy()
        ));
    };
    if is_this_program(&path) {
        return Err(format!(
            "{WRAPPER_VAR} names read-after-link itself ({})",
            path.display()
        ));
    }
    let mut command = Command::new(path);
    command.arg(compiler).args(args);
    Ok(command)
}

/// The program a wrapper names: a path as given, or a bare name found on `PATH` (and only there:
/// Windows would look in this program's own directory first).
fn locate(program: &OsStr) -> Option<PathBuf> {
    let path = Path::new(program);
    if path.is_absolute() || path.components().count() > 1 {
        return path.is_file().then(|| path.to_path_buf());
    }
    env::split_paths(&env::var_os("PATH")?)
        .flat_map(|dir| candidates(&dir.join(path)))
        .find(|candidate| candidate.is_file())
}

/// The files a name in a `PATH` directory may be: on Windows, with each `PATHEXT` extension when
/// it has none of its own.
fn candidates(path: &Path) -> Vec<PathBuf> {
    if !cfg!(windows) || path.extension().is_some() {
        return vec![path.to_path_buf()];
    }
    env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned())
        .split(';')
        .filter(|extension| !extension.is_empty())
        .map(|extension| {
            let mut name = path.as_os_str().to_owned();
            name.push(extension);
            PathBuf::from(name)
        })
        .collect()
}

/// Whether `program` is this executable: a wrapper that names it would run itself forever.
fn is_this_program(program: &Path) -> bool {
    match (
        env::current_exe().and_then(fs::canonicalize),
        fs::canonicalize(program),
    ) {
        (Ok(this), Ok(that)) => this == that,
        _ => false,
    }
}

/// The exit code to pass back: the compiler's, or on Unix 128 plus the signal that ended it.
fn exit_code(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128_i32.saturating_add(signal);
        }
    }
    1
}

/// The arguments with each argument file (`@path`, one argument per line) replaced by its lines,
/// as rustc reads them. A file that cannot be read stays as it is (it may be some other tool's).
fn expand(args: &[OsString]) -> Vec<OsString> {
    let mut expanded = Vec::with_capacity(args.len());
    for arg in args {
        match arg.to_str().and_then(|arg| arg.strip_prefix('@')) {
            Some(path) if !path.starts_with("shell:") => match fs::read_to_string(path) {
                Ok(contents) => expanded.extend(contents.lines().map(OsString::from)),
                Err(_) => expanded.push(arg.clone()),
            },
            _ => expanded.push(arg.clone()),
        }
    }
    expanded
}

/// What the wrapper needs from a rustc command line.
#[derive(Debug, Default, PartialEq)]
struct Compile {
    crate_name: Option<String>,
    crate_types: Vec<String>,
    extra_filename: String,
    out_dir: Option<PathBuf>,
    /// `-o`.
    output: Option<PathBuf>,
    /// The kinds of every `--emit`.
    emit: Vec<String>,
    /// `--emit link=<path>`.
    emit_link: Option<PathBuf>,
    /// The input is standard input (`-`): cargo's probes of the compiler.
    reads_stdin: bool,
}

fn parse(args: &[OsString]) -> Compile {
    let mut compile = Compile::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let Some(text) = arg.to_str() else {
            continue;
        };
        let (flag, inline) = split_flag(text);
        match flag {
            "--crate-name" | "--crate-type" | "--out-dir" | "--emit" | "-o" | "-C"
            | "--codegen" => {
                let value = match inline {
                    Some(value) => OsString::from(value),
                    None => match args.next() {
                        Some(value) => value.clone(),
                        None => break,
                    },
                };
                apply(&mut compile, flag, value);
            }
            "-" => compile.reads_stdin = true,
            _ => {}
        }
    }
    compile
}

/// A flag and its attached value: `--flag=value`, `-Cvalue`, `-ovalue`.
fn split_flag(arg: &str) -> (&str, Option<&str>) {
    if arg.starts_with("--") {
        return match arg.split_once('=') {
            Some((flag, value)) => (flag, Some(value)),
            None => (arg, None),
        };
    }
    for short in ["-C", "-o"] {
        if let Some(value) = arg.strip_prefix(short) {
            if !value.is_empty() {
                return (short, Some(value));
            }
        }
    }
    (arg, None)
}

fn apply(compile: &mut Compile, flag: &str, value: OsString) {
    match flag {
        "--crate-name" => compile.crate_name = value.into_string().ok(),
        "--crate-type" => {
            if let Some(types) = value.to_str() {
                compile
                    .crate_types
                    .extend(types.split(',').map(str::to_owned));
            }
        }
        "--out-dir" => compile.out_dir = Some(value.into()),
        "-o" => compile.output = Some(value.into()),
        "--emit" => {
            for entry in value.to_str().unwrap_or_default().split(',') {
                let (kind, path) = match entry.split_once('=') {
                    Some((kind, path)) => (kind, Some(path)),
                    None => (entry, None),
                };
                if kind == "link" {
                    if let Some(path) = path {
                        compile.emit_link = Some(path.into());
                    }
                }
                compile.emit.push(kind.to_owned());
            }
        }
        // `-C` and `--codegen`.
        _ => {
            if let Some(extra) = value
                .to_str()
                .and_then(|option| option.strip_prefix("extra-filename="))
            {
                extra.clone_into(&mut compile.extra_filename);
            }
        }
    }
}

/// The dynamic library a compile links, if it links one.
fn library(compile: &Compile) -> Option<PathBuf> {
    if compile.reads_stdin
        || !compile
            .crate_types
            .iter()
            .any(|crate_type| DYNAMIC.contains(&crate_type.as_str()))
        || (!compile.emit.is_empty() && !compile.emit.iter().any(|kind| kind == "link"))
    {
        return None;
    }
    if let Some(path) = compile.emit_link.as_ref().or(compile.output.as_ref()) {
        return Some(path.clone());
    }
    let file = format!(
        "{}{}{}{}",
        env::consts::DLL_PREFIX,
        compile.crate_name.as_ref()?,
        compile.extra_filename,
        env::consts::DLL_SUFFIX
    );
    Some(compile.out_dir.clone().unwrap_or_default().join(file))
}

/// Reads a newly linked library on Windows (and reports it to the log everywhere).
fn settle(library: &Path) {
    let shown = library.display();
    if cfg!(windows) {
        match read_whole(library) {
            Ok(bytes) => log(&format!("read\t{bytes}\t{shown}")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                log(&format!("missing\t{shown}"));
            }
            Err(error) => {
                eprintln!("read-after-link: warning: could not read {shown}: {error}");
                log(&format!("failed\t{error}\t{shown}"));
            }
        }
    } else if library.is_file() {
        log(&format!("found\t{shown}"));
    } else {
        log(&format!("missing\t{shown}"));
    }
}

/// Reads a file from start to end, returning its length.
fn read_whole(path: &Path) -> io::Result<u64> {
    let mut file = File::open(path)?;
    let mut buffer = vec![0; CHUNK];
    let mut total = 0_u64;
    loop {
        match file.read(&mut buffer) {
            Ok(0) => return Ok(total),
            Ok(read) => total = total.saturating_add(u64::try_from(read).unwrap_or(u64::MAX)),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

/// Appends a line to `READ_AFTER_LINK_LOG`, when it names a file. Each line is one write.
fn log(line: &str) {
    let Some(path) = env::var_os(LOG_VAR).filter(|path| !path.is_empty()) else {
        return;
    };
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(format!("{line}\n").as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &[&str]) -> Vec<OsString> {
        line.iter().map(OsString::from).collect()
    }

    fn dll(dir: &str, stem: &str) -> PathBuf {
        Path::new(dir).join(format!(
            "{}{stem}{}",
            env::consts::DLL_PREFIX,
            env::consts::DLL_SUFFIX
        ))
    }

    /// A proc macro as cargo compiles it.
    const PROC_MACRO: &[&str] = &[
        "--crate-name",
        "derive_more_impl",
        "--edition=2021",
        "src/lib.rs",
        "--error-format=json",
        "--json=diagnostic-rendered-ansi,artifacts,future-incompat",
        "--crate-type",
        "proc-macro",
        "--emit=dep-info,link",
        "-C",
        "prefer-dynamic",
        "-C",
        "embed-bitcode=no",
        "-C",
        "debuginfo=2",
        "-C",
        "metadata=0a1b2c3d4e5f6a7b",
        "-C",
        "extra-filename=-27ab1c8e9f0d3e41",
        "--out-dir",
        "target/debug/deps",
        "-L",
        "dependency=target/debug/deps",
        "--extern",
        "proc_macro2=target/debug/deps/libproc_macro2-1111111111111111.rlib",
        "--extern",
        "proc_macro",
        "--cap-lints",
        "allow",
    ];

    #[test]
    fn a_proc_macro_links_its_library() {
        assert_eq!(
            library(&parse(&args(PROC_MACRO))),
            Some(dll(
                "target/debug/deps",
                "derive_more_impl-27ab1c8e9f0d3e41"
            ))
        );
    }

    #[test]
    fn attached_values_count_too() {
        let line = [
            "--crate-name=pm",
            "--crate-type=lib,proc-macro",
            "-Cextra-filename=-x",
            "--out-dir=out",
            "lib.rs",
        ];
        assert_eq!(library(&parse(&args(&line))), Some(dll("out", "pm-x")));
        let line = [
            "--crate-name",
            "pm",
            "--crate-type",
            "cdylib",
            "--codegen",
            "extra-filename=-y",
            "lib.rs",
        ];
        assert_eq!(library(&parse(&args(&line))), Some(dll("", "pm-y")));
        let line = [
            "--crate-name",
            "pm",
            "--crate-type",
            "dylib",
            "--codegen=extra-filename=-z",
        ];
        assert_eq!(library(&parse(&args(&line))), Some(dll("", "pm-z")));
    }

    #[test]
    fn only_dynamic_libraries_count() {
        for crate_type in ["lib", "rlib", "staticlib", "bin"] {
            let line = [
                "--crate-name",
                "c",
                "--crate-type",
                crate_type,
                "--out-dir",
                "o",
            ];
            assert_eq!(library(&parse(&args(&line))), None, "{crate_type}");
        }
        let line = ["--crate-name", "c", "--out-dir", "o", "main.rs"];
        assert_eq!(library(&parse(&args(&line))), None);
    }

    #[test]
    fn a_compile_that_links_nothing_reads_nothing() {
        let line = [
            "--crate-name",
            "pm",
            "--crate-type",
            "proc-macro",
            "--emit=dep-info,metadata",
            "--out-dir",
            "o",
        ];
        assert_eq!(library(&parse(&args(&line))), None);
    }

    #[test]
    fn an_explicit_output_wins() {
        let line = [
            "--crate-name",
            "pm",
            "--crate-type",
            "proc-macro",
            "-o",
            "x/pm.dll",
        ];
        assert_eq!(
            library(&parse(&args(&line))),
            Some(PathBuf::from("x/pm.dll"))
        );
        let line = [
            "--crate-name",
            "pm",
            "--crate-type",
            "proc-macro",
            "-ox/pm.dll",
        ];
        assert_eq!(
            library(&parse(&args(&line))),
            Some(PathBuf::from("x/pm.dll"))
        );
        let line = [
            "--crate-name",
            "pm",
            "--crate-type",
            "proc-macro",
            "--emit",
            "dep-info,link=y/pm.dll",
            "-o",
            "x/pm.dll",
        ];
        assert_eq!(
            library(&parse(&args(&line))),
            Some(PathBuf::from("y/pm.dll"))
        );
    }

    #[test]
    fn cargo_probes_read_nothing() {
        let line = [
            "-",
            "--crate-name",
            "___",
            "--print=file-names",
            "--crate-type",
            "bin",
            "--crate-type",
            "rlib",
            "--crate-type",
            "dylib",
            "--crate-type",
            "cdylib",
            "--crate-type",
            "staticlib",
            "--crate-type",
            "proc-macro",
            "--print=sysroot",
        ];
        assert_eq!(library(&parse(&args(&line))), None);
        assert_eq!(library(&parse(&args(&["-vV"]))), None);
        assert_eq!(library(&parse(&[])), None);
    }

    #[test]
    fn a_wrapped_driver_is_just_another_argument() {
        // `RUSTC_WRAPPER RUSTC_WORKSPACE_WRAPPER rustc …`: the wrapper sees `rustc` first.
        let mut line = vec!["/toolchain/bin/rustc"];
        line.extend_from_slice(PROC_MACRO);
        assert_eq!(
            library(&parse(&args(&line))),
            Some(dll(
                "target/debug/deps",
                "derive_more_impl-27ab1c8e9f0d3e41"
            ))
        );
    }

    #[test]
    fn a_truncated_command_line_reads_nothing() {
        let line = ["--crate-type", "proc-macro", "--out-dir"];
        assert_eq!(library(&parse(&args(&line))), None);
    }

    #[test]
    fn argument_files_are_read_as_rustc_reads_them() {
        let dir = scratch("argfile");
        let file = dir.join("args");
        fs::write(&file, PROC_MACRO.join("\n")).expect("the argument file is written");
        let line = vec![OsString::from(format!("@{}", file.display()))];
        assert_eq!(
            library(&parse(&expand(&line))),
            Some(dll(
                "target/debug/deps",
                "derive_more_impl-27ab1c8e9f0d3e41"
            ))
        );
        // Windows line endings, as an editor may leave them.
        fs::write(&file, PROC_MACRO.join("\r\n")).expect("the argument file is written");
        assert_eq!(
            library(&parse(&expand(&line))),
            Some(dll(
                "target/debug/deps",
                "derive_more_impl-27ab1c8e9f0d3e41"
            ))
        );
        // Another tool's response file that is not text stays an argument.
        fs::write(&file, [0xff, 0xfe, b'/', 0]).expect("the response file is written");
        assert_eq!(expand(&line), line);
        let missing = vec![OsString::from("@no/such/file")];
        assert_eq!(expand(&missing), missing);
        fs::remove_dir_all(dir).expect("the scratch directory is removed");
    }

    #[test]
    fn read_whole_reads_every_byte() {
        let dir = scratch("read");
        let file = dir.join("library");
        let length = CHUNK * 3 + 17;
        fs::write(&file, vec![7_u8; length]).expect("the file is written");
        assert_eq!(
            read_whole(&file).expect("the file is read"),
            u64::try_from(length).expect("the length fits")
        );
        assert_eq!(
            read_whole(&dir.join("missing")).map_err(|error| error.kind()),
            Err(io::ErrorKind::NotFound)
        );
        fs::remove_dir_all(dir).expect("the scratch directory is removed");
    }

    #[test]
    fn a_bare_name_is_found_on_path_only() {
        let dir = scratch("locate");
        assert_eq!(locate(dir.join("absent").as_os_str()), None);
        let program = dir.join(if cfg!(windows) { "tool.exe" } else { "tool" });
        fs::write(&program, b"").expect("the program is written");
        assert_eq!(locate(program.as_os_str()), Some(program.clone()));
        assert_eq!(locate(OsStr::new("read-after-link-no-such-program")), None);
        fs::remove_dir_all(dir).expect("the scratch directory is removed");
    }

    /// A new directory for one test.
    fn scratch(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("read-after-link-unit-{name}-{}", process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("the scratch directory is created");
        dir
    }
}
