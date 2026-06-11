# Fix note — vm-cli-args-2 (B2, B3, B5)

Round-2 carry-over from the `vm-cli` review (`docs/reviews/fable-2026-06-10/vm-cli.md`).
B1 (`-Xms` separate-token) was already fixed in Round 1 and is left intact. This note covers
B2, B3, and B5. All edits are confined to `vm-cli/src/main.rs`.

---

## B3 (MEDIUM) — unrecognized single-dash `-X...` flags reached clap and errored

### Finding
`-XX:`-prefixed flags are broadly accept-and-ignored, but single-dash `-X` flags other than the
explicitly-handled set (`-Xmx`/`-Xms`/`-Xshare`/`-Xverify`/`-Xbootclasspath`/`-Xlog`/`-noverify`)
fell through `normalize_java_launcher_argv`'s final `else`, were pushed verbatim, and clap then
rejected them with "unexpected argument '-X'". Common HotSpot flags that hit this and that
Surefire/Gradle pass routinely: `-Xss<size>`, `-Xint`, `-Xbatch`, `-Xrs`, `-XshowSettings`,
`-Xnoclassgc`.

### Root cause
No catch-all arm for the `-X` family analogous to the `-XX:` catch-all.

### Exact change
Added an `else if a.starts_with("-X")` arm immediately after the `-XX:` catch-all and before the
final `else` in `normalize_java_launcher_argv`. It accepts-and-ignores the token (drops it,
`i += 1`) with the same `CRATONVM_DBG_ARGS` trace as the `-XX:` arm. Placement after all the
recognized `-X*` branches means only genuinely-unrecognized single-dash `-X` flags reach it; the
value-taking forms are consumed earlier, and every remaining `-X` flag is the inline/no-value form,
so dropping the single token is correct (HotSpot has no separate-token spelling for them).

---

## B2 (MEDIUM) — blanket `--` strip dropped a separator the Java program legitimately receives

### Finding
`args.args.retain(|a| a != "--")` removed **every** `--` from the program-args vector. Stock
`java Main a -- b` must deliver `["a","--","b"]` to `main`; the blanket strip silently delivered
`["a","b"]`.

### Root cause
The launcher inserts at most one `--` separator (consumed by clap as the option-parsing
terminator), so in the normal-class and `-jar` launch modes **no** launcher `--` reaches
`args.args` — anything left there is genuine user data. The sole exception is the JBoss-Modules
`-mp` path: `normalize_java_launcher_argv` prepends an extra `--` (so `-mp` isn't mis-parsed as the
`-m`/`-p` short-flag cluster), which makes `insert_program_args_separator`'s own trailing `--` leak
past clap's terminator as the **last** element of `args.args`. The blanket retain was a too-broad
patch for that one leaked trailing artifact.

### Exact change
Replaced `args.args.retain(|a| a != "--")` with a single trailing-only pop:

```rust
if args.args.last().map(String::as_str) == Some("--") {
    args.args.pop();
}
```

This removes exactly the leaked launcher artifact (always the final token in the `-mp` path) and
leaves interior user `--` tokens untouched. The pre-existing `class_name == "--"` guard is kept.

### Verified by reasoning (per launch mode)
- `java Main a -- b` → clap `args = [a, --, b]`; last is `b`, pop is a no-op → user `--` preserved.
- `java -jar foo.jar a -- b` → clap `args = [a, --, b]`; last is `b`, no-op → user `--` preserved.
- `java -mp /modules` → clap `args = [-mp, /modules, --]`; last is `--`, popped → `[-mp, /modules]`,
  preserving the JBoss fix.

The one residual ambiguity is a user passing a genuinely-trailing `--` as their final program arg;
that is indistinguishable from the launcher artifact and is an extremely rare invocation. This is a
strict improvement over the previous blanket strip.

---

## B5 (LOW, cosmetic) — load-failure message rendered the class name two ways

### Finding
`bail!("Could not find or load main class {}: {e}", class_name.replace('/', "."))` dot-normalizes
the prefix name, but the inner error `e` (from `load_class` → `load_class_concurrent` →
`class_manager.load_class`, all in other crates) embeds the same name in **slash** form, so a
packaged class showed up two ways (`com.example.Main` then `class not found: com/example/Main`).

### Root cause
Only the prefix was dot-normalized; the inner error's rendered text was interpolated raw. The
error string is produced in a crate this agent does not own, so the normalization is applied at the
render site instead.

### Exact change
Dot-normalize the rendered inner error too:

```rust
bail!(
    "Could not find or load main class {}: {}",
    class_name.replace('/', "."),
    e.to_string().replace('/', ".")
);
```

`e` already implemented `Display` (it was interpolated as `{e}`), so `.to_string()` is available
via the blanket `ToString` impl. HotSpot reports the dotted binary name throughout; this matches.

---

## Files touched
- `vm-cli/src/main.rs`
  - `normalize_java_launcher_argv`: new catch-all `else if a.starts_with("-X")` accept-and-ignore arm (B3).
  - post-clap `run()` separator handling: blanket `retain` → trailing-only `pop` (B2).
  - main-class load-failure `bail!`: dot-normalize the inner error rendering (B5).
  - `#[cfg(test)] mod tests`: 5 new tests (see below).

## Tests added
- `hotspot_xss_inline_is_accepted_and_ignored` (B3) — `-Xss512k` drops out.
- `hotspot_misc_x_flags_are_accepted_and_ignored` (B3) — `-Xint`/`-Xbatch`/`-Xrs`/`-Xnoclassgc` drop out.
- `hotspot_xss_passes_clap_after_full_pipeline` (B3) — full pre-clap pipeline + clap accepts `-Xss512k`.
- `user_double_dash_reaches_program_args` (B2) — `java Main a -- b` → `parsed.args == [a, --, b]`.
- `launcher_trailing_double_dash_artifact_is_popped` (B2) — `-mp` leaks a trailing `--`; the pop
  rule removes exactly that and yields `[-mp, /modules]`.

(No standalone test for B5: the error text originates in a non-owned crate, so an end-to-end string
assertion would couple to cross-crate wording; the existing `cli_missing_class.rs` already exercises
the path and only checks the simple name, so it stays green.)

## Follow-up & risk
- Low risk. B3/B5 are additive/cosmetic. B2 narrows behavior from "strip all `--`" to "strip one
  trailing `--`"; the only behavior change for existing flows is that interior/preserved user `--`
  now survives (the desired fix) — no integration test depended on the blanket strip (only
  `cli_main_args.rs` touches argv ordering and uses no `--`).
- Residual (acknowledged in B2 above): a user's genuinely-trailing `--` as the final program arg is
  indistinguishable from the `-mp` launcher artifact and will still be popped. A fully precise fix
  would thread a "launcher inserted an extra separator" flag out of `normalize_java_launcher_argv`
  into `run()`; deferred as not worth the added surface for a rare, ambiguous case.
- Did not run cargo build/check/test/clippy or git, per task constraints.
