# Fuzz regressions

One directory per target (`parser/`, `terminal/`, `chunking/`), each holding
minimised inputs that once crashed it, named after their issue
(`<issue>-<what>`). The nightly fuzz workflow replays every file before
fuzzing; locally:

```sh
cargo +nightly fuzz run <target> fuzz/regressions/<target>/*
```

Add an input when its crash is filed (minimise it first with
`cargo +nightly fuzz tmin <target> <artifact>`), so the fix is checked and the
crash cannot come back unnoticed.
