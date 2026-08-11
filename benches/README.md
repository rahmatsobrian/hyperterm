# Benchmarks

Deliberately implemented as plain Rust binaries under `examples/` instead of
`cargo bench` + `criterion`, to avoid taking on extra dependency/MSRV risk
just for benchmarking (see CONTRIBUTING.md "Toolchain notes" for why that's
a real concern in this project's dependency tree).

Run:

```powershell
cargo run --release --example bench_scrollback
cargo run --release --example bench_ansi_parse
```

CI runs both in the `benchmark` job of `.github/workflows/build.yml` on
every push/PR.
