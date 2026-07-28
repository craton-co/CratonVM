# JASPER-JDT.2 / JASPER-JDT.3 (Eclipse JDT parser + AST) — FIXED and REMOVED 2026-07-28

**Status: ✅ CLOSED.** Both bans are deleted from `vm/src/jit/skip_list.rs`.
The defect behind them is root-caused: the LICM / speculative pre-header bypass
fixed by `613b10f4c`. This is the third and final entry in the file's history —
the first removal (2026-07-26) was void, the restore (2026-07-27) was correct,
and this removal is backed by a bisect and a same-binary causal A/B rather than
by another "no longer reproduces".

## What the two bans were

Both were real Eclipse JDT (ECJ) miscompiles, discovered through Tomcat's
internal use of ECJ to compile JSPs.

* **JASPER-JDT.2** (`org/eclipse/jdt/internal/compiler/parser/`, 2026-07-08) —
  `org.apache.jasper.compiler.TestCompiler` hit nondeterministic
  parser-adjacent heap corruption / OOM under JIT; first face an
  `ArrayIndexOutOfBoundsException` in `Parser.parse`, bisected to
  `Parser.consumeRule` and the parser package generally.
* **JASPER-JDT.3** (`org/eclipse/jdt/internal/compiler/ast/`, 2026-07-10) — a
  second, independent family in the AST / flow-analysis package. Real Tomcat
  FORM-auth repro (`TestFormAuthenticatorA/B/C` forwarding to the login-page
  JSP): intermittent `JasperException` whose root cause was an
  `ArrayIndexOutOfBoundsException` reported at
  `QualifiedNameReference.analyseCode` — a trivial delegating wrapper with no
  array access of its own.

## The 2026-07-26 removal was void, and the 2026-07-27 restore was right

Both were removed on 2026-07-26 on four repeat runs each of the real Tomcat
integration suites. Every one of those runs was made while
`helpers::direct_virtual_compiled_callee_entry_enabled()` was **default-OFF**.
That flag gates the only write of `mic.cached_entry_ptr`, so with it off a
JIT-compiled caller's `invokevirtual` never reaches a JIT-compiled callee at
all. The compiled-to-compiled virtual dispatch this family lives in was inert
for the whole verification: those runs could not have reproduced the defect
whatever state it was in.

Turning the flag on (2026-07-27, for the H2 `TestFreeSpace` / `TestNestedJoins`
residuals) brought the family straight back with a new face — real Tomcat
`jakarta.el.TestOptionalELResolverInJsp`, whose JSP compile died with

```
ClassCastException: org.eclipse.jdt.internal.compiler.ast.QualifiedTypeReference
  cannot be cast to org.eclipse.jdt.internal.compiler.ast.FieldDeclaration
  -> JasperException: Unable to compile class for JSP -> HTTP 500
```

Both bans were restored: `parser/` directly re-confirmed (`CRATONVM_JIT_DENY`
on `parser/` restored PASS; denying `ast/`, `lookup/` or `util/` did not),
`ast/` on the shadowing argument alone.

## Root cause: the LICM / speculative pre-header bypass (`613b10f4c`)

### Bisect

Restore point `4f280090f`, rebuilt from scratch (fat LTO, default profile),
both packages JIT-allowed, direct-entry path at its default (ON):
`jakarta.el.TestOptionalELResolverInJsp` **3/3 FAIL**, identical CCE. Current
`dev` on the same harness: **3/3 PASS**.

Bisected the 150 commits between them, two runs per step, always with
`CRATONVM_JIT_ALLOW_PACKAGES=org/eclipse/jdt/internal/compiler/parser/,org/eclipse/jdt/internal/compiler/ast/`
and never touching `CRATONVM_JIT_DISPATCH_CACHE_VIRTUAL_DIRECT_ENTRY`:

| commit | verdict |
|---|---|
| `4f280090f` (restore point) | BROKEN 3/3 |
| `bc68152be` | BROKEN |
| `c5e82fefa` | BROKEN |
| `10adad90e` | BROKEN |
| `255bb0a8c` | BROKEN |
| `0313a7208` | build failure → skipped |
| `613b10f4c` | **FIXED** — first clean commit |
| `1db9562b1` | FIXED |
| `017bc3734` | FIXED |
| `2094aba74^` | FIXED |

`613b10f4c` = *"fix(jit): LICM/speculative pre-header bypassed by a branch into
the loop header"*.

Its immediate parent `c042e794f` cannot be built — `dev` was briefly
non-compiling there because a merge dropped
`NativeContext::invoke_special_by_class_id`, which `613b10f4c` itself restores
(same reason `0313a7208` had to be skipped). The bisect verdict therefore rests
on the five buildable BROKEN ancestors above, none of which contains
`613b10f4c`, and the four FIXED points, all of which do.

### Causal A/B, one binary, current dev

Bisect names a commit; this names the change. A current-`dev` binary was built
with an env-gated one-line revert of the fix — `find_bypassable_loop_headers`
returns the empty set under `CRATONVM_EXP_NO_BYPASS_GUARD`, i.e. every
speculative pre-header is emitted again exactly as before `613b10f4c`
(experiment only, never merged):

| config (same binary, same fixture, JDT allowed) | result |
|---|---|
| `CRATONVM_EXP_NO_BYPASS_GUARD=1` (pre-fix behaviour) | **2/2 FAIL**, identical CCE |
| unset (shipping behaviour) | **2/2 PASS** |

### Why the mechanism fits

Every speculative pre-header the x86-64 backend emits — LICM hoists *and*
speculative bounds-check-elision guards, plus SIMD batch pre-headers — is
emitted inline **at the loop-header PC**, and `pc_to_native[header]` is then set
past it so the back edge does not re-run it. That contract assumes the only way
into the loop is the linear fall-through. A forward branch that jumps straight
into the header lands *after* the pre-header, so the loop runs against an
uninitialised hoist slot, or with bounds checks elided by a guard that never
executed.

ECJ's generated `Parser` and its AST/flow-analysis code are full of exactly that
shape. An unchecked array read handing back the wrong live object is precisely a
`QualifiedTypeReference` (or `ParameterizedQualifiedTypeReference` — both faces
were observed) arriving where a `FieldDeclaration` was expected, and it is
precisely the `ArrayIndexOutOfBoundsException` / heap-corruption faces the two
bans were originally opened for in 2026-07-08 and 2026-07-10.

It also explains the family's notorious nondeterminism without any appeal to
races: what the loop reads on a bypassing edge is whatever the previous call
left in that frame slot.

### What it is *not*

* **Not** the NodeConnections retired-code SIGSEGV (`2094aba74`). A build of
  that commit's parent is already clean 3/3, so the JDT failure was gone before
  it landed.
* **Not** the still-open json-smart corruption on the same dispatch path (see
  the `jit-virtual-direct-entry-json-corruption-20260727` write-up). That one
  needs a *moving young generation* and is VM-wide rather than JDT-specific; it
  is unaffected either way by whether these two packages are JIT-eligible, and
  keeping a JDT ban is not a mitigation for it. Recorded here only so the two
  are not conflated again.

## Re-verification with the bans deleted

Binary: `cratonvm` built from the branch with both prefixes deleted from
`skip_list.rs`, default release profile (fat LTO), **no**
`CRATONVM_JIT_ALLOW_PACKAGES`, **no** `CRATONVM_JIT_DISPATCH_CACHE_VIRTUAL_DIRECT_ENTRY`
override — i.e. exactly the shipping configuration.

Not-a-no-op check first (`CRATONVM_DBG_DUMP_JIT=LIST`, one run): **167**
`org/eclipse/jdt/internal/compiler/parser/` methods and **29**
`org/eclipse/jdt/internal/compiler/ast/` methods actually compiled.

| suite | tests | runs |
|---|---|---|
| `jakarta.el.TestOptionalELResolverInJsp` | 1 | 3/3 OK |
| `org.apache.catalina.authenticator.TestFormAuthenticatorA` | 9 | 2/2 OK |
| `org.apache.catalina.authenticator.TestFormAuthenticatorB` | 6 | 2/2 OK |
| `org.apache.catalina.authenticator.TestFormAuthenticatorC` | 7 | 2/2 OK |
| `org.apache.jasper.compiler.TestCompiler` | 12 | 2/2 OK |

The same battery was also run on an unmodified `dev` binary with the two
packages lifted via `CRATONVM_JIT_ALLOW_PACKAGES` (3/3, 2/2, 2/2, 2/2, 2/2) —
same result from the other direction.

`cargo test -p cratonvm-vm --lib skip_list`: 69 passed, 0 failed.

Repeated after merging the 27 `dev` commits that landed during the session,
on a binary rebuilt from the merged tree — 167 parser + 29 ast methods still
compiling, `TestOptionalELResolverInJsp` 2/2, `TestFormAuthenticatorA` 1/1,
`TestCompiler` 1/1, `cargo test -p cratonvm-vm --lib skip_list` 69/69.

## Residual: the `webresources` `checkPath` IAE

A prior session recorded, as "the JASPER-JDT.3 residual", a distinct JIT-only
`IllegalArgumentException` from
`org/apache/catalina/webresources/AbstractResourceSet.checkPath` — *"The
requested path [/WEB-INF/...] is not valid. It must begin with /"* for a path
that visibly does begin with `/`, i.e. `path.charAt(0) != '/'` evaluating true
when it should not. It was never banned, only flagged.

**Not observed in any run of this session** — 0 occurrences across the 26
Tomcat runs above and the ~20 bisect runs, all with the JDT packages compiled.
That is an absence, not a proof: nothing here targeted it, and it was
intermittent when filed. It is plausibly the same defect (an elided bounds check
/ stale hoist slot on a bypassed loop entry is exactly how a correct string
reads back wrong), but that was not measured. Left as-is for whoever meets it
again, with the note that current `dev` does not show it.

## Reproduction

```bash
cd /data/data/apps/tomcat            # relative webapp paths do not resolve elsewhere
CP=$(cat .suite/cp-linux-fixed.txt)
<cratonvm> --java-home /home/victor/jdk25 -Xmx2g -cp "$CP" \
  org.junit.runner.JUnitCore jakarta.el.TestOptionalELResolverInJsp
```

~20–60 s. `TestFormAuthenticatorA/B/C` are minutes; `TestCompiler` is 7–9 min,
so use a 600 s+ timeout for it. To exercise the packages on a binary that still
carries the bans, add
`CRATONVM_JIT_ALLOW_PACKAGES=org/eclipse/jdt/internal/compiler/parser/,org/eclipse/jdt/internal/compiler/ast/`
— note that `ALLOW_PACKAGES` entries must be **broader-or-equal** than the ban's
own prefix, so these two exact strings are required; a sub-prefix silently lifts
nothing and reports a false clean.

## The rule this leaves behind

Unchanged from the restore, and now demonstrated twice: any ban whose mechanism
is compiled-to-compiled virtual dispatch must be re-verified with
`CRATONVM_JIT_DISPATCH_CACHE_VIRTUAL_DIRECT_ENTRY` at its default (**on**), or
it verifies nothing. And a removal that rests only on "no longer reproduces" is
worth much less than one that can name the commit and the code change that fixed
it — the first removal of these two bans looked just as careful as this one.
