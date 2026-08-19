# FIXED — `NativeImageHandlerMetadataTest` × 17 now pass: the harness shows each one its own module, the way Maven does

**Status:** ✅ FIXED 2026-08-19 on `fix/netty-dns-and-nativeimage-metadata-20260819`
(branched from `dev` at `c1be69779`). Retires
`docs/known-issues/netty/nativeimagehandlermetadatatest-not-a-cratonvm-bug-20260819.md`,
which correctly established that this cluster is not a VM defect and then left
the harness gap open. The gap is now closed: **17 FAIL → 17 PASS on CratonVM,
matching HotSpot exactly.**

## What was broken

17 classes, one per netty module, all named `NativeImageHandlerMetadataTest`,
each with one method (`collectAndCompareMetadata`), failed identically on
CratonVM and on stock HotSpot 25:

```
org.opentest4j.AssertionFailedError: Native Image reflection metadata is
required for handlers in this project. This metadata was not found under
/data/.../apps/netty-suite-runner/src/main/resources/META-INF/native-image
/null/null/generated/handlers/reflect-config.json
```

`ChannelHandlerMetadataUtil.generateMetadata` asks Reflections for every
`ChannelHandler` subtype under a package prefix and compares that set against
the module's checked-in `reflect-config.json`. Three separate things about the
flat suite run break it, and the open page had only found the first two:

1. **`null/null`.** The path is built from `nativeImage.handlerMetadataGroupId`
   and `nativeimage.handlerMetadataArtifactId`, which netty's `pom.xml:1682-1683`
   feeds to surefire from `${project.groupId}`/`${project.artifactId}`. This
   harness never runs `mvn`, so both `System.getProperty` calls return `null`
   and the two path segments are the literal string `null`.
2. **The path is relative.** `new File("src/main/resources/...")` resolves
   against the process working directory — the module directory under Maven,
   `$HERE` (the fixture dir) under `run-netty-suite.sh`.
3. **The flat classpath over-collects** — this is the part that made the
   obvious fix insufficient. `common.args` puts *every* module's
   `target/classes` + `target/test-classes` on one classpath. Scanning
   `io.netty.handler.codec` for `codec-base` then sees every `codec-*` module's
   handlers; scanning `io.netty.channel` for `transport` sees
   `transport-native-unix-common-tests`' handlers, which the util's
   `/test-classes/` filter does not drop because that module's tests are its
   *main* classes. Maven never shows those to the module, so the checked-in file
   rightly does not list them. Fixing only 1 and 2 turns "file not found" into a
   long "the following new metadata must be added" diff — verified, not assumed.

## The fix

Three tracked files next to `run-netty-suite.sh`, plus the wiring to use them.

**`module-scoped-classes.tsv`** — the 17 classes with their module directory and
Maven coordinates:

```
io.netty.channel.NativeImageHandlerMetadataTest	transport	io.netty	netty-transport
io.netty.handler.codec.NativeImageHandlerMetadataTest	codec-base	io.netty	netty-codec-base
...
```

**`gen-module-args.sh`** — regenerates `module-args/<artifactId>.args`, one per
entry, each holding that single module's Maven test classpath:

```
mvn -o -q -pl <module> dependency:build-classpath -DincludeScope=test
```

prefixed with the module's own `target/classes` + `target/test-classes` and
suffixed with the fixture dir and the JUnit Platform Launcher jar (surefire owns
the launcher under Maven, so no netty module declares it). The launcher must be
version-matched to the module's `junit-platform-commons`: the local repo also
holds a JUnit 6 launcher, and pairing that with netty's 1.14.x platform rejects
every class up front with "conflicting versions were detected". The argfiles are
host-specific absolute paths, exactly like `common.args`, so they are generated
rather than tracked; `run-netty-suite.sh` rebuilds any that are missing instead
of silently running these classes wrong.

**`run-netty-suite.sh`** — a class with a `module-scoped-classes.tsv` entry runs
with the module directory as cwd and its own argfile in place of `common.args`.
Every run prints `module-scope=N (loaded)`; `run-netty-suite.sh module-scope`
prints the table; `--no-module-scope` disables it for A/B.

## Result

All 17, through the real harness, on the branch build:

```
@@RESULT io.netty.channel.NativeImageHandlerMetadataTest found=1 started=1 ok=1 failed=0 aborted=0 skipped=0
... × 17
--- summary(cv): 17 clean of 17 results
--- summary(hs): 17 clean of 17 results
```

And the A/B that shows the scoping is what does it:

```bash
./run-netty-suite.sh --list /tmp/ab.txt --shards 2                     # status: PASS=2
./run-netty-suite.sh --list /tmp/ab.txt --shards 2 --no-module-scope   # status: FAIL=2
```

(`/tmp/ab.txt` = `io.netty.channel` + `io.netty.handler.codec`, the transport
and codec-base entries — the two worst over-collectors.)

## Why this was worth doing rather than excluding the classes

The open page's fallback was to drop all 17 from the class lists as out-of-scope
for a non-Maven harness, the same treatment as the quarkus `*IT` classes. That
would have been defensible and would have cost 17 classes of coverage
permanently. It is also not quite the same case: the `*IT` classes need a
packaged artifact that does not exist here, whereas everything these 17 need
already exists on disk — the `reflect-config.json` files are all present under
`<module>/src/main/resources/META-INF/native-image/io.netty/<artifactId>/generated/handlers/`.
Only the harness's view of them was wrong.

There is also a real, if incidental, VM signal in the result: these 17 exercise
Reflections' classpath scanning, gson deserialization and `Class.getResource`
across 17 different module shapes, and CratonVM now matches HotSpot on all of
them.

## Files

- `apps/netty-suite-runner/module-scoped-classes.tsv` (new, tracked)
- `apps/netty-suite-runner/gen-module-args.sh` (new, tracked)
- `apps/netty-suite-runner/run-netty-suite.sh` (module-scope wiring)

All force-added past `.gitignore:12`'s blanket `apps/` ignore, the same way
`apps/hib-suite-runner/run-hib.sh` and `apps/h2database-suite-runner/run-h2-suite.sh`
already are. Before this change the whole netty runner was untracked, while its
own header comment claimed `class-overrides.tsv` was "tracked in git".

## Related

- `dnsnameresolvertest-windows-only-aborts-CONFIRMED-20260819.md` — the other
  harness gap closed in the same change.
- `partial-run-fail-hang-triage-20260817.md` — the quarkus `*IT` classes, the
  "needs real build-tool output" case this one turned out not to be.
