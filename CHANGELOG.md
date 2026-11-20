# Changelog

## 0.1.1

- On Windows, a compile whose command line, with its argument files (`@path`) expanded, is longer
  than 30,000 characters runs without `READ_AFTER_LINK_WRAPPER`, with its arguments as given: the
  compiler reads the argument files itself. sccache expands argument files and could not start such
  a compile ("The filename or extension is too long. (os error 206)",
  [mozilla/sccache#3053](https://github.com/mozilla/sccache/issues/3053)); since cargo 1.100, crates
  with large dependency graphs get there (one `-L dependency=` per dependency's build directory).
  The log notes each such compile: `direct<TAB>characters<TAB>crate`.
- The README gives the numbers of the confirmation run.

## 0.1.0

- First release.
