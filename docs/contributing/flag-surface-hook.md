# The flag-surface pre-push hook

`.githooks/pre-push` runs the three `CRATONVM_*` flag-surface guards before a
push leaves your machine. It is **opt-in per clone**:

```bash
git config core.hooksPath .githooks
```

To turn it off again:

```bash
git config --unset core.hooksPath
```

## Why this exists when CI already runs the same tests

It does — `.github/workflows/ci.yml`'s `cargo test --workspace` covers all
three, and that has never been the gap. The gap is *when*.

This repository is worked in dozens of local worktrees whose branches are
merged into `dev` and pushed directly. CI therefore runs **after** `dev` has
already moved: it can report red, it cannot prevent it. Four undeclared flags
have reached `dev` in a single day before —
`CRATONVM_SYNTHETIC_MXBEAN_MAPPING`, `CRATONVM_SYNTHETIC_MEMORYUSAGE_TOSTRING`,
`CRATONVM_DBG_OVERLAY_GATE` and
`CRATONVM_JIT_NO_PRECISE_GETSTATIC_CHECKCAST` — each found by whoever branched
next and had to stop and fix someone else's flag before their own work could be
verified.

The hook closes exactly the window CI cannot: between "committed locally" and
"on a branch other people build from".

## What it runs, and what it costs

```
cargo test -q -p cratonvm-types \
  --test flag_declaration_guard \
  --test flag_docs_generated \
  --test flag_surface
cargo test -q -p cratonvm-types --lib flag_groups
```

Warm, about **3 s** (2 s for the targets, 1 s for the lib filter). That number
is the design constraint, not a footnote: a
push-time gate that costs minutes gets `--no-verify`d permanently and is worse
than no gate. Everything here is a source scan and a table comparison — no VM
boots, no fixtures, no network, no non-determinism.

Those three targets cover the flag surface in **both** directions:

* a flag **read but not declared** — `every_cratonvm_literal_is_declared_or_explicitly_exempt`;
* a flag **declared but no longer read** — `every_declared_flag_still_has_a_reader`,
  added 2026-09-04. A knob whose last reader was deleted or renamed still
  appears in `docs/CONFIG.md` and in the generated tables, where it reads as a
  supported lever that does nothing. This half previously lived only in
  `tools/flag-census/check-surface.sh`, which runs on the Linux CI leg *after*
  the push — so it reported the problem and could not prevent it. It costs
  nothing extra here: the scan it needs has already been walked for the first
  check.

  It found `CRATONVM_JIT_IR_GATED_REF_STORE` on the day it was written, left
  behind when its reader was renamed to `CRATONVM_JIT_IR_REF_STORE`.

The second invocation exists because the first cannot reach the invariants that
are cheapest to get wrong. `flag_groups`'s unit tests check the INVENTORY table
against *itself* - a row expands to something, `off_word` appears only on a
default-ON knob with no opt-out key, no legacy variable is claimed by two
tokens - and they live in the types crate's **lib**, which `--test <target>`
never runs.

That gap was not hypothetical. On 2026-09-04 three `CRATONVM_XT_*` rows named
the same variable as both `on_key` and `off_key`, violating two of those
invariants at once; they reached `dev` through this hook and stayed red until
someone tripped over them while validating an unrelated merge.

Folding `--lib` into the first invocation would run the whole types lib -
measured **24 s** against the targets' 2 s, which is the kind of number that
gets a hook `--no-verify`d permanently. Filtered to `flag_groups` in its own
invocation it is ~1 s warm. (A first measurement said 8 s; that was a cold build
of a test binary this hook had never built, not the steady state.)

Note what "read" means: a name counts as read only where it is **read**, not
where it is declared. `flag_groups.rs` is itself a Rust source, so every
`on_key: Some("CRATONVM_X")` puts that literal in the scan — and without
excluding the declaration site the reverse check passes for every possible
input. It did, on first writing, with a knob that had no reader at all
deliberately re-added.

For the same reason the scope is the flag surface and nothing else. **It is not
a substitute for running the tests your change actually touches.**

## Skipping it

```bash
CRATONVM_SKIP_FLAG_HOOK=1 git push ...   # skip just this check
git push --no-verify ...                 # skip every hook
```

The first exists so you can bypass *this* gate without also bypassing whatever
hook someone adds later.

The hook also exits 0 without running anything for a delete-only push
(`git push origin --delete <branch>`), and if `cargo` is not on `PATH` it says
so on stderr and lets the push through — refusing a docs-only push from a
machine with no toolchain would be worse than the problem it is guarding.

## Why `core.hooksPath` and not `.git/hooks`

`.git/hooks` is per-clone and unversioned, so a hook placed there is invisible
to review and to everyone else. `.githooks/` is committed, reviewable, and
shared; `core.hooksPath` is the one line that points git at it.

That line is local configuration and cannot be committed, which makes the hook
opt-in by construction. That is deliberate — a hook is code that runs on
someone else's machine, and `core.hooksPath` is repository-wide, so enabling it
takes effect in **every worktree of this clone at once**, including any a
concurrent session is mid-push in.

## When it fires

The failure message carries the fix, in order. The short version:

1. `types/src/flag_groups.rs` — add an `E { .. }` row to `INVENTORY`.
2. The read site — if it uses `std::env::var`, move it onto the crate's
   snapshot accessor and add the typed field in `types/src/flags.rs`. If it
   already uses `flags::runtime_var[_os]`, **step 1 is the whole fix**: that
   function serves declared names from the latched snapshot by itself and only
   falls through to a live `getenv` for an undeclared one.
3. `types/tests/flag-surface.txt` — add the name, sorted.
4. Regenerate both docs, never hand-edit them:
   ```bash
   python tools/flag-census/render-inventory.py .
   bash   tools/flag-census/render-tokens.sh
   ```
   `render-inventory.py` refuses to run if step 3 was skipped, which is how it
   keeps the fixture and the docs from drifting apart.
