# The `[site-alias]` diagnostic, and the question it was really asking — CLOSED 2026-08-06

**Status:** CLOSED. Both halves answered; neither needs further work.

Filed earlier the same day as
`known-issues/jit/site-alias-diagnostic-prints-to-stdout-and-flakes-the-strict-corpus-20260806.md`
with two parts: a diagnostic print that reached stdout and flaked the strict
corpus gate, and — the part the doc said not to silence without answering —
whether the thing it reported, a recycled `JitSiteKey`, could reach a cache.

## Half one: the print. Fixed by a concurrent session, verified here

`note_site_identity` is gated on `CRATONVM_DBG_SITE_ALIAS`, and the gate lives
at the *call sites* rather than inside, so an all-off run does not make the call
at all. **One of the three call sites had the comment but not the `if`** —
`vm/src/jit/helpers.rs`, the raw-entry native dispatch path — so it printed on
every raw-entry native dispatch in an ordinary run and grew the unbounded
`SITE_IDENTITY` map alongside.

`5d299ca6f` added the missing guard. That commit is **not** an ancestor of
`66725787e`, the dev tip this was measured against, which is why the measurement
and the fix look contradictory: they were true at different commits, hours
apart. Its message names the same evidence independently —
"caught by `scripts/jdk-only-strict-probes.sh`, which saw the lines in BOTH
CratonVM arms of `JdkOnlyPlatformProbe`".

Verified against `origin/dev` @ `5d1a4dfe5` rather than taken from the diff:

```text
[site-alias] occurrences per run, CRATONVM_DBG_SITE_ALIAS unset:
  0 0 0 0 0 0 0 0 0 0        (was 1/6 runs on 66725787e, 5/6 on a rebuild)

with CRATONVM_DBG_SITE_ALIAS=1 — the diagnostic still works:
  [cratonvm] site-alias: distinct JitSiteKeys=6 recycled-key hits=0

strict corpus gate, three consecutive runs:
  RESULT: PASS / PASS / PASS   (was PASS, then FAIL, same binary)
```

The rate detail from the original filing — 1/6 on dev's binary versus 5/6 on a
rebuilt one — is explained by the same thing: both binaries predated the guard,
and the key in the message is an address, so how often a key collides tracks JIT
code-cache layout, which any code change shifts. It was never about the change
that happened to be in the tree.

## Half two: can a recycled `JitSiteKey` reach a cache?

**It could, that was a real defect (`383e7f5cf`), and it is closed.** The
argument, which was in the code but not written anywhere a reader would find it:

1. A `JitSiteKey` is `(vm_identity, JitInvokeInfo address)`, and a
   `JitInvokeInfo` box is owned by its `CompiledMethod`
   (`CompiledMethod::_jit_invoke_infos`) — so the address is freed and can be
   re-issued.
2. Every per-thread memo keyed on such an address is declared through the
   `site_keyed_memos!` macro, which generates `clear_site_keyed_dispatch_memos`
   **from the same declaration list**. That is what `383e7f5cf` fixed: four of
   the eight memos sat on neither hand-maintained flush list, so one call site
   served another's dispatch. A memo now cannot exist without being flushed, and
   the compiler enforces it rather than a reviewer.
3. `flush_raw_entry_dispatch_caches()` runs at the top of both dispatch entry
   points, *before* any memo probe, and clears everything when
   `jit_cache_generation()` or `jit_supersede_epoch()` has moved.
4. **The closing step:** a recycled address can only become *dispatchable*
   through a publication — compiled code referencing the new `JitInvokeInfo` has
   to be published before it can run — and publication bumps
   `JIT_CACHE_GENERATION` **unconditionally**, first-time insertions included
   ("T2.2 — bump on EVERY publication, not just replacements"). So the flush
   always lands between an address being re-issued and any dispatch through it.

The one case that looks like a hole is not one: a compile that **bails** frees
its `owned_invoke_infos` with no publication and no bump. But nothing was ever
published that references those addresses, so no compiled code can dispatch
through them and no memo can be keyed on them. They are freed unobserved.

`a_jit_generation_change_clears_every_site_keyed_memo` already pins step 3 — and
pins it against `site_keyed_memo_census()`, the real list, so a memo added later
is covered the moment it is declared.

## Step 4 is now pinned too

It was the one part of the chain resting on a comment plus four hand-written
`fetch_add` calls rather than on a test.
`every_publication_advances_the_jit_cache_generation` (`jit/src/lib.rs`) closes
it, over all three publication shapes:

* **first-time insertion** — the one a replacement-only bump would miss, which is
  why the bump site calls it out ("bump on EVERY publication, not just
  replacements");
* **replacement** — the case that actually frees the previous artifact's
  `JitInvokeInfo` boxes, i.e. the one the whole argument is about;
* **`put_osr`** — a separate entry point with its own bump.

Two properties keep it honest. Each arm asserts the body is **reachable**
afterwards, not merely that a counter moved: a `put` that declines (stale
publication epoch, `prepare_for_publication` refusal) publishes nothing and
correctly does not bump, so a counter-only test could pass while proving nothing
about publication. And the comparison is strictly-greater rather than
`== before + 1`, because `JIT_CACHE_GENERATION` is a process global and the test
binary runs in parallel — `==` would be a flake, not a stronger assertion.

**Verified as a negative control**, which is the difference between a guard and a
decoration: deleting the `fetch_add` at the first-time publication site turns the
test red on the first arm, with the message it was written to produce —

```text
a first-time publication did not advance the JIT cache generation; a recycled
JitInvokeInfo address could then inherit the previous site's memoized dispatch
```

## The reason this doc is not just deleted

The filing said "do not silence the print without first answering whether a
recycled key can reach a cache." The print was silenced — correctly — and the
answer existed only as scattered code comments. Section two is that answer,
written down once, so the next person who meets `[site-alias]` does not re-derive
it or assume the guard made the question go away.
