# SEAM-02 — split the interpreter dispatch files (24,474 + 26,049 lines)

**Status:** not started. **Owns:** `../../../vm/src/runtime/interpreter.rs`,
`vm/src/runtime/interpreter/*`. **Independent of `seam-01`** — different crate,
no shared file.

## Why

`../../../vm/src/runtime/interpreter/invoke.rs` is 24,474 lines and
`../../../vm/src/runtime/interpreter.rs` is 26,049. Costs observed this campaign, all
specific:

* The JVMTI delivery lane's handover census said "~15 call sites in
  `interpreter.rs`". The real count was **28, and 8 of them were in
  `invoke.rs`** — a file the census never mentioned. Trusting it would have
  left the entire cached-invoke and OSR surface unattributed while looking
  complete.
* A second door to the JIT: the OSR path in `invoke.rs` calls the backend
  directly rather than through `try_compile`, so anything `try_compile` does at
  entry is silently skipped there. Found only because another lane needed a
  witness at compile start.
* `constants.rs` was split out of `interpreter.rs` by an earlier commit, which
  invalidated a handover recipe written days earlier that still said
  "interpreter.rs". The recipe's author could not have known.

The precedent already exists — `interpreter/` is a directory with
`constants.rs`, `invoke.rs` and siblings — so this is continuing a split, not
starting one.

## Candidate seams

| Candidate | Content |
|---|---|
| `interpreter/dispatch_virtual.rs` | virtual/interface dispatch, the inline-cache consult, the superclass walk that intercepts natives |
| `interpreter/dispatch_static.rs` | static/special, and the call-site evidence recording `pgo-01` needs |
| `interpreter/jit_bridge.rs` | every site that enters, exits, or requests compiled code — including the direct OSR compile |
| `interpreter/jvmti_events.rs` | the 33 attributed delivery sites |
| `interpreter/exceptions.rs` | throw, handler search, the deopt signal drain |

`jit_bridge.rs` is the highest-value one: "every place the interpreter talks to
the JIT" is currently not enumerable, and that is exactly the property the
second-door bug exploited.

## How to do it without breaking anything

Pure moves, one seam per commit, `vm` suite green at each step. Note that
`../../../vm/src/lib.rs` is `#![deny(deprecated)]`, so a move that makes a deprecated
call newly visible fails the build rather than warning — that is a feature
here, but it will surprise you once.

## How to verify

The `vm` unit suite (2,431 tests) at every commit, plus a real app suite —
these files are the interpreter's hot path and a subtle behaviour change will
show up in Spring or Tomcat long before it shows up in a unit test.

## What to refuse

Behaviour changes in a move commit. In particular, do not "tidy" the native
override priority while moving `dispatch_virtual.rs`: a registered native wins
over real bytecode unconditionally, natives on abstract classes intercept
every subclass, and both facts have live dependents.
