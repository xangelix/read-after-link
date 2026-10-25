# read-after-link

A rustc wrapper (`RUSTC_WRAPPER`) that reads each newly linked proc-macro DLL once, start to end,
before cargo starts anything that loads it. It works around a Windows race in which a fresh DLL is
loaded with pages of zeros, which shows up in Windows CI as random compiler crashes. It is
especially likely when the build goes through sccache.

```sh
cargo install --locked read-after-link
```

## The problem

On Windows, a proc macro compiled moments earlier sometimes fails when rustc loads it for the
next crate. The failure looks different each time:

- `fatal runtime error: failed to set FLS value`, then exit code `0xc0000409`
- `STATUS_ACCESS_VIOLATION` (`0xc0000005`), or a stack overflow, in rustc
- ``error[E0463]: can't find crate for `derive_more_impl` ``, naming a crate cargo has just compiled
- `LoadLibraryExW failed` (`ERROR_NOACCESS`)

The bytes on disk are correct. What is wrong is the copy Windows maps into memory: some pages of
the DLL's code and data are zeros. In the first case, the key of Rust's thread-local destructor
guard, which starts as `0xFFFFFFFF` in `.data`, reads as 0. The standard library then takes it for
a key it has allocated, `FlsSetValue` fails, and it aborts. Once
it has happened, every later load of that file fails the same way until the file is replaced. A
copy of the same bytes loads correctly.

It happens when the DLL was written through a memory-mapped view (as the MSVC linker writes it)
and its first cached read overlaps its first load as an executable image. Plain cargo builds
hardly ever do that, because nothing reads a proc macro but the compiler that loads it.
sccache does. For each cacheable crate, its server hashes every `--extern` file while, at the
same moment, a probe compile loads the same proc macros. A read that has completed before the
first load prevents the problem, and so does `FlushFileBuffers` after the write.

This wrapper makes sure such a read always comes first. After a compile that links a dynamic
library succeeds, the wrapper reads the library once before it exits. Cargo starts nothing that
uses the library until the compile has exited, so the read completes before anything can load it.

### Where it was seen

The failures were seen on Windows Server 2025 (build 26100), in a process-isolated container
whose build directory is mounted from the host. Similar failures were reported in 2023 in
[rust-lang/rust#107595](https://github.com/rust-lang/rust/pull/107595), also in a Windows
container with a target directory shared with the host.

### Measurements

| | Failed |
| --- | --- |
| CI builds through sccache from a clean target directory | 9 of about 45 |
| The same, with this read after each proc macro links | 0 of about 45 |
| Standalone (no cargo, sccache, or rustc), in the mounted build directory: load racing the first read | 5 of 1,250 |
| Standalone, mounted directory, with the read completed before the load | 0 of 875 |
| Standalone, mounted directory, with `FlushFileBuffers` after the write | 0 of 500 |
| Standalone, the container's own disk: load racing the first read | 0 of 500 (too few to tell) |

The standalone reproduction writes a DLL through a mapped view, reads it in 128 KiB chunks the
way sccache hashes, and loads it in another process with
`LoadLibraryExW(DONT_RESOLVE_DLL_REFERENCES)`. It then compares every section with the file,
leaving out base relocations and the `/GS` cookie the loader writes.

This is a workaround. The bug is in Windows, and the right fix belongs there.

## Usage

Set it as cargo's rustc wrapper:

```powershell
$env:RUSTC_WRAPPER = (Get-Command read-after-link).Source
```

With sccache (or any other wrapper), put it in front and name the other wrapper in
`READ_AFTER_LINK_WRAPPER`:

```powershell
$env:RUSTC_WRAPPER = (Get-Command read-after-link).Source
$env:READ_AFTER_LINK_WRAPPER = (Get-Command sccache).Source
```

### Keeping C and C++ on sccache

The [`cc`](https://crates.io/crates/cc) crate sends C and C++ compiles through `RUSTC_WRAPPER`
only when that file is named `sccache` (or `cachepot`, `buildcache`, `kache`). To keep that
behavior, install a copy of this wrapper named `sccache.exe` in a directory of its own. Point
`RUSTC_WRAPPER` at the copy, and `READ_AFTER_LINK_WRAPPER` at the real sccache by its full path:

```powershell
New-Item -ItemType Directory -Force C:\tools\read-after-link | Out-Null
Copy-Item (Get-Command read-after-link).Source C:\tools\read-after-link\sccache.exe
$env:RUSTC_WRAPPER = 'C:\tools\read-after-link\sccache.exe'
$env:READ_AFTER_LINK_WRAPPER = (Get-Command sccache).Source  # the real one, by full path
```

The copy passes C and C++ compiles to sccache unchanged: they link no Rust library, so it reads
nothing. If `READ_AFTER_LINK_WRAPPER` names the wrapper itself, it refuses to run rather than run
itself forever.

## Configuration

| Variable | Meaning |
| --- | --- |
| `READ_AFTER_LINK_WRAPPER` | Another wrapper to run the compiler through, such as sccache. A full path is used as given; a bare name is looked up on `PATH`. |
| `READ_AFTER_LINK_LOG` | A file to append one line to per library: `read<TAB>bytes<TAB>path`, `found<TAB>path` (other platforms), `missing<TAB>path`, or `failed<TAB>error<TAB>path`. |

`read-after-link --version` and `--help` work when given as the only argument.

## What it does exactly

- It runs `<compiler> <arguments>`, or `$READ_AFTER_LINK_WRAPPER <compiler> <arguments>`, with the
  same standard input, output, and error, and exits with the compiler's exit code. It doesn't
  interpret the output.
- It reads the library only after a successful compile whose crate types include `proc-macro`,
  `dylib`, or `cdylib`, and only on Windows. On other platforms it only passes compiles through,
  and the log says `found`.
- It finds the library the way rustc names it: `--out-dir`, `--crate-name`, and
  `-C extra-filename`, or `-o`, or `--emit link=<path>`. It reads argument files (`@path`) as
  rustc does. Cargo's probes of the compiler (input `-`) and compiles that don't link
  (`--emit=metadata`) read nothing.
- It never fails a compile that succeeded. A read error is a warning on standard error, which
  cargo shows.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
