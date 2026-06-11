# Fix: vm-cli-args — B1 `-Xms <size>` separate-token form corrupts the argv pipeline

## Finding
B1 (HIGH) from `docs/reviews/fable-2026-06-10/vm-cli.md`: the separate-token
`-Xms <size>` form (heap min as a *following* argv token, e.g. `java -Xms 512m Main`)
was mishandled in `normalize_java_launcher_argv`. Only the `-Xms` token was dropped;
the bare value token (`512m`) survived. Downstream, clap mistook `512m` for the
main-class positional and `Main` became a stray second positional, so the launch
either errored with "unexpected argument" or tried to run the wrong "class" `512m`.
The inline form `-Xms512m` already worked (the value rides on the dropped token),
which masked the bug. Maven Surefire / Gradle forks commonly emit the separate-token
form, which is exactly why `-Xms` is listed in `VALUE_TAKING_OPTS`.

## Root cause
`vm-cli/src/main.rs`, `normalize_java_launcher_argv` `-Xms` branch (was line ~808):

```rust
else if a.starts_with("-Xms") {
    ...
    i += 1;   // consumes ONLY "-Xms"; the separate value token is left standing
}
```

`-Xms` is accepted-and-ignored (CratonVM sizes the heap from `-Xmx` only), so unlike
the sibling `-X` options it is not rewritten to a clap long option — it is simply
dropped. The branch unconditionally advanced by one, which is correct for the inline
form (`-Xms512m`, single token) but wrong for the separate-token form (`-Xms` `512m`,
two tokens), where the value must also be consumed.

## Exact change (file:line)
`vm-cli/src/main.rs` — `-Xms` branch in `normalize_java_launcher_argv` (now ~820-831):
distinguish the two forms before advancing, mirroring the existing
`-Xshare`/`-Xverify`/`-Xbootclasspath`/`-Xlog` separate-token branches:

```rust
else if a.starts_with("-Xms") {
    if std::env::var_os("CRATONVM_DBG_ARGS").is_some() {
        eprintln!("[cratonvm] ignoring unimplemented HotSpot flag: {a}");
    }
    if a == "-Xms" && i + 1 < args.len() {
        // Separate-token form `-Xms 512m`: drop the value token too.
        i += 2;
    } else {
        // Inline form `-Xms512m`, or a bare trailing `-Xms`: drop just this token.
        i += 1;
    }
}
```

The `a == "-Xms"` exact-match gate means inline `-Xms512m` (where `a != "-Xms"`) and a
bare trailing `-Xms` with no value (where `i + 1 >= args.len()`) both still advance by
one and drop only the flag — so B4 (bare trailing `-Xms`) is also handled without
over-consuming.

## Sibling split-token audit (per task instruction)
Reviewed every value-taking `-X`/`-XX:` option in the same loop for the same bug:
- `-Xmx` (checks `rest.is_empty()` → `i += 2` for separate token) — correct.
- `-Xshare` / `-Xverify` / `-Xbootclasspath` / `-Xlog` (explicit `a == "-X..." && i + 1 < len` → `i += 2`) — correct.
- Single-dash `-XX:` value forms are inline-only (`-XX:Foo=val`); no separate-token path.

`-Xms` was the **only** option with this defect. No other flag was touched.

## Tests added (`#[cfg(test)] mod tests` in the same file)
Mirrors the existing `hotspot_xmx_*` test style (uses the existing `argv`,
`insert_program_args_separator`, `Args::try_parse_from` helpers):
1. `hotspot_xms_inline_is_dropped_keeping_class_name` — `-Xms512m Main` → `["java","Main"]`.
2. `hotspot_xms_separate_token_drops_value_not_class_name` — full pre-clap path drops
   the `512m` value, keeps `Main` (the core B1 regression guard).
3. `hotspot_xms_separate_token_passes_clap_after_full_pipeline` — end-to-end through
   clap: `-Xms 512m -Xmx 256m -classpath x Main` parses with `class_name == "Main"`,
   `max_heap == "256m"`, `classpath == "x"`.

Each expected output was hand-traced through `insert_program_args_separator` +
`normalize_java_launcher_argv` + the extract stages to confirm correctness.

## Follow-up & risk
- Risk: low. Change is confined to one branch; behavior for the inline and
  bare-trailing forms is byte-identical to before (still `i += 1`). Only the
  previously-broken separate-token path changes.
- Not addressed (out of scope for B1, separate findings in the report): B2 (blanket
  `--` strip at line ~1067), B3 (other `-X` single-dash flags like `-Xss` reach clap),
  B5 (cosmetic load-failure message). These touch the same file but are independent
  findings; left for their own fixes.
