# `apps/{netty,hib}-suite-runner` infra files — the wider exposure flagged in the `class-overrides.tsv` doc, closed

**Status: FIXED 2026-09-07** for all nine files listed below.

## Background

See `class-overrides-tsv-lost-to-a-cleanup-commit-FIXED-20260906.md` for the
full story of `e33f6d7e3` ("cleanup", 2026-08-29): it deleted 368 files from
git, most of them genuine scratch cruft, but swept up several files that were
deliberately `git add -f`'d past `apps/`'s wholesale `.gitignore` because they
are infrastructure, not scratch. That doc restored the three
`class-overrides.tsv` files and flagged the rest as a "wider exposure ... out
of scope for this triage pass":

```
apps/netty-suite-runner/module-scoped-classes.tsv
apps/netty-suite-runner/run-netty-suite.sh
apps/netty-suite-runner/gen-module-args.sh
apps/netty-suite-runner/gen-openssl-args.sh
apps/netty-suite-runner/CratonRunner.java
apps/netty-suite-runner/known-benign-aborts.tsv
apps/netty-suite-runner/README.md
apps/hib-suite-runner/run-hib.sh
apps/hib-suite-runner/known-benign-aborts.tsv
```

This page closes that gap: each file's git-history version
(`e33f6d7e3^`) was diffed against its content on the shared Azure host
(`azureuser@20.80.105.49:/data/cratonvm/apps/{netty,hib}-suite-runner/`) —
the one place any post-deletion fix could have landed, since no commit since
`e33f6d7e3` has re-added any of them — and the **current, live** content was
force-added to git. `apps/hib-suite-runner/README.md` was checked too but
never existed in git history or on the host; it is not part of this
restoration.

## What the diff actually found

Two outcomes, verified by SHA-256 rather than assumed:

### Untouched since the deletion (verbatim restore, no drift)

`gen-module-args.sh`, `gen-openssl-args.sh`, and `hib-suite-runner/run-hib.sh`
hash-matched `e33f6d7e3^` byte-for-byte on the host. The speculative note in
the `class-overrides.tsv` doc ("`run-netty-suite.sh` in particular has real
fixes layered on top of the pre-deletion version") turned out to apply to
`run-netty-suite.sh` itself, not `run-hib.sh` — `run-hib.sh` has had no
functional edits since 2026-08-29 despite an Aug 31 mtime (a touch/rebuild
artifact, not a content change).

Three more files — `CratonRunner.java`, `known-benign-aborts.tsv`, and
`README.md`, all under `netty-suite-runner/`, plus
`hib-suite-runner/known-benign-aborts.tsv` — were **not just missing from
git, they no longer existed on the live host at all.** Only a compiled
`CratonRunner.class` (no `.java`) remained; `run-netty-suite.sh` and
`run-hib.sh` were both silently printing `WARNING: known-benign-aborts table
not found` and falling back to the flat "everything not literally PASS is a
FAIL" categorization — the same failure shape the `class-overrides.tsv` doc
described, just for a different table. A handful of stale pre-deletion
worktree checkouts still on the host (`/data/cvm-l2s-ctrl/...`, dated
2026-08-28, the day before the deletion) confirmed these four files were
byte-identical to `e33f6d7e3^` at the moment they were lost — they are git
checkouts of that same history, not independent host edits, so they add
confirmation but not new information. All four were restored verbatim from
the git blob, to both git and back onto the live host.

### Genuinely drifted (current host content wins)

**`run-netty-suite.sh`** (536 → 577 lines) gained two things since
`e33f6d7e3^`:
- An explicit `--module-scope` flag (paired with the pre-existing
  `--no-module-scope`) to force module-scoped-classpath execution rather than
  only being able to disable it.
- A reactor-built sanity check (dated 2026-09-04 in its own comment): a
  missing `*/target/classes` directory on a Java classpath is silently
  skipped, not an error, so an unbuilt Maven reactor doesn't fail loudly —
  every class whose module wasn't built just reports `found=0 tests` and the
  run finishes looking clean. 683/733 classes silently reported 0 tests this
  way once, after a checkout refresh wiped every module's `target/` with
  nothing rebuilding it; only `netty-transport`'s classes ran, because its
  own stale test-jar was pulled in transitively. The check now counts how
  many `*/target/classes` classpath entries actually exist and refuses to
  run (`exit 1`) if fewer than half do, rather than produce a summary that
  reads as a completed suite.

**`module-scoped-classes.tsv`** (60 → 61 lines) gained one entry:
`io.netty.util.internal.NativeLibraryLoaderTest` → module `common`. This is
the same fix already described in dev commit `996c6d829`'s triage doc
(`NativeLibraryLoaderTest`, CWD-resolution mechanism, FIXED) — that fix had
landed on the host but, like everything else on this page, only on the host.

Both were force-added with their **current host content**, not the
pre-deletion git version — a blind `git checkout e33f6d7e3^ -- <path>` would
have silently reverted both of these.

## Fix

All nine files force-added to git (`apps/`'s blanket `.gitignore` requires
`-f`, same as the `class-overrides.tsv` restoration). Executable bits set to
match how the scripts are actually invoked (`chmod 755` on the four `.sh`
files) — `run-hib.sh` and `gen-openssl-args.sh` were tracked as `100644` in
the pre-deletion history despite being run directly; `run-netty-suite.sh` and
`gen-module-args.sh` were already `100755` and unchanged. The four
fully-missing files (`CratonRunner.java`, both `known-benign-aborts.tsv`
lost, `netty-suite-runner/README.md`) were also written back to the live
host so the runners stop warning about a missing table. Verified after:
`run-netty-suite.sh benign-aborts` now reports `state: 8 (loaded)` for
`known-benign-aborts.tsv` (previously `table not found`).

No further host-only drift is known to remain in these two directories —
every file the original deletion touched has now either been confirmed
identical to git history or had its live edits captured.

## Related

* `class-overrides-tsv-lost-to-a-cleanup-commit-FIXED-20260906.md`
* `docs/known-issues/netty/nativeimagehandlermetadatatest-harness-module-scope-FIXED-20260819.md`
