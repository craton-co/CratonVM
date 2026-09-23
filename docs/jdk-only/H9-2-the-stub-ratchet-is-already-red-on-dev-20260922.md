# H9-2 — the stub ratchet is red on `dev`, and the delta is the HOST, not a commit

**Status: ✅ ATTRIBUTED and FIXED 2026-09-22.** The +38 was not introduced by any
commit. **The commit that froze the baseline measures it red on this machine.**
The gate had one number per feature configuration and none per platform, and a
Windows build registers ~40 `SyntheticStub` natives a Linux build does not
compile at all.

The first version of this page said the delta was "not attributable from here"
and left it for whoever made the 31 (later 58) `native-builtins/src` commits in
the range. That was the wrong lane to send it to: the range contains no such
commit.

## The failure

```
cargo test --release -p cratonvm-native-builtins --test stub_ratchet
```

```
STUB-RATCHET in the no-management configuration: 4952 SyntheticStub natives
now registered, exceeding the frozen baseline of 4914.
...
total registrations are 13889, and this baseline was measured beside 13863.
```

## The measurement that settles it

`1748c259e` is the commit that wrote
`BASELINE_SYNTHETIC_STUBS_NO_MANAGEMENT = 4914`, on 2026-09-21. Checked out into
a detached worktree and run with `CRATONVM_RATCHET_ROWS=1`, **its own tree**
measures:

| tree | stubs | total registrations |
|---|---|---|
| frozen in `1748c259e`'s source | 4914 | 13863 |
| `1748c259e` **run here** | **4954** | **13878** |
| `HEAD` (58 commits later) run here | **4952** | **13889** |

So the +40 was already there at the freeze, and the commits since are worth
**−2 stubs and +11 rows** — a small net *reduction* in fakes, which is the
direction the gate wants. (58 of those commits touch `native-builtins/src`,
72 touch any of the `native-*` crates; neither number matters once the delta is
-2.) Reading the earlier +38 as a regression by those
commits would have been a bisect over a delta they did not cause.

The same +38 appears in the `management` configuration (4979 against a frozen
4941, totals 14261 against 14235), which is what a *configuration*-independent
cause looks like.

## The mechanism, not a hypothesis

`native-builtins` registers natives for classes that exist only on one
platform, inside ordinary `#[cfg]`:

* `native-builtins/src/attach_provider.rs` is `#![cfg(windows)]` in its
  entirety, and says why in its own header: "The Linux class of the same name
  declares neither of these methods, which is why every registration below is
  `#![cfg(windows)]`."
* `phases_late/nio_file.rs` has `#[cfg(windows)]` blocks registering
  `sun/nio/fs/WindowsNativeDispatcher` — `initIDs`, `GetFileAttributesEx0` and
  the whole `win32_register_path_native!` family — none of which a Linux build
  compiles.

Counted in the row dump: **140 of the 4954 stub rows name a Windows class**
against 23 naming a Unix one. The registry is platform-shaped, so a single
frozen count cannot be right on both platforms; whichever host did not produce
it sees a gate that is red for reasons no diff can explain.

## The fix

`native-builtins/tests/stub_ratchet.rs` grows a **platform axis** beside the
feature axis it already had. Every existing constant keeps its value and its
history — they are the numbers measured on the project's Linux host and nothing
here re-freezes them — and a parallel set of `PLATFORM_*` constants carries the
Windows measurements. The selection picks by `cfg(target_os)` exactly as it
already picks by `cfg(feature)`, and `MEASURED_PLATFORM` is printed beside
`MEASURED_CONFIG` in the failure message, so a number can no longer be pasted in
from a run of the other host any more than from a run of the other
configuration — which is the failure the `MEASURED_CONFIG` label was itself
added for ("The 1038 this file carried until 2026-08-11 was six above the 1032
the same code measured").

MEASURED on this host, `HEAD` of this branch:

| configuration | stubs | total registrations |
|---|---|---|
| `management` | 4979 | 14261 |
| no-management | 4952 | 13889 |
| `synthetic-jdk` | 4952 | 13900 |

## What this does NOT do

It does not re-freeze the non-Windows numbers, and nothing here says they are
wrong: they were measured where they were measured, and this branch cannot run
that host. If they have drifted, that is a separate finding and this change
makes it *more* visible, not less, because a Linux run is no longer adjudicated
against a number a Windows run pasted over it.

It also does not claim the two platforms *should* differ by exactly 40. The
number is a measurement, not a derivation; the derivation would be an inventory
of every `#[cfg]`-gated registration, which is a bigger exercise and one the
row dump now makes mechanical for whoever wants it.

## The other gate in the same session

`jit/tests/process_global_statics_ratchet.rs` was red on `dev` too, and that one
**was** a commit: `473df2db1` added `HASHMAP_GET_DIRECT_SITES_OSR` and
`HASHMAP_PUT_DIRECT_SITES_OSR` without moving the baseline. Attribution was
exact — the scanner's own rule, replayed over `git ls-tree` at four revisions,
counts 831 at `56616250e` and at `473df2db1^`, and 833 at `473df2db1` and at
`HEAD`, with exactly one commit in the 53 adding a counted declaration. Raised
to 833 with that paragraph, per the file's own "the next raise owes the same
paragraph" rule.

The two gates went red for opposite reasons and only one of them was anybody's
regression. That is the argument for the platform label: a gate that cannot say
which environment produced its number cannot tell those two apart.
