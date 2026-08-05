# Sustain hostile-input fuzzing

This detached workspace holds focused, manually invoked fuzz targets for
hostile-by-accident music-library inputs. It is deliberately excluded from
Sustain's normal workspace and commit gate: fuzz campaigns are bounded
maintenance runs, while every genuine finding gets a minimized GitHub issue
and eventually a deterministic regression test with its fix.

Requirements:

```sh
rustup toolchain install nightly --profile minimal
cargo install cargo-fuzz --locked
```

Build both targets from the repository root:

```sh
cargo +nightly fuzz build
```

Run the managed-path planner campaign:

```sh
cargo +nightly fuzz run managed_path_plan -- \
  -max_total_time=1800 -timeout=5 -max_len=65536
```

Run the SQLite metadata/path round-trip campaign:

```sh
cargo +nightly fuzz run sqlite_track_roundtrip -- \
  -max_total_time=1800 -timeout=5 -max_len=131072
```

Do not repair a failure directly from its raw artifact. Reproduce it, minimize
it with `cargo +nightly fuzz tmin`, rule out a duplicate, and open a GitHub
issue that records the violated invariant, impact, minimized input, Sustain
commit, tool versions, and exact command. Production fixes are separate work.
