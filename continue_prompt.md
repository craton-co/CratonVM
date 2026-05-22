# Continue prompt — CratonVM real-Java-app gauntlet

You are continuing work on **CratonVM**, a JVM written in Rust at `C:\craton\CratonVM`.
Binary: `target/release/java.exe`. Real JDK 25 at `C:/Program Files/Java/jdk-25`.
Build: `cargo build --release --bin java` (~6 min). Working branch: `dev`.

## Goal

Every app in `apps/` must **start AND pass its e2e tests** on CratonVM running real
`.class` bytecode, **no synthetic stubs** in the VM (no fake-main shims, no faked
JDK classes — fix the real VM bug). As an app passes, delete it from `apps/` and
refill from `apps/TARGET_APPS.md`.

## CURRENT STATE — `dev` is at `d6a0542`, GREEN and verified

Verified on `dev`: cassandra `NodeTool version` rc=0 (5/5 runs), tomcat boots
("Server startup in N ms"), kafka prints real USAGE, activemq + keycloak26 pass.
Already landed: kafka TreeSet$Itr.remove fix, tomcat `alloc_synthetic` fix, the
solr triage-classpath correction, 8 integrated recovered fixes, and `fix-wildfly2`
(WildFly/Keycloak16 boot fixes).

## #1 PRIORITY — the cassandra `MetadataLoader` reflection bug (THE GATE)

Four fix branches are ready but **every one regresses cassandra** identically:
```
java.lang.NullPointerException: Cannot invoke iterator on null
  at io.airlift.airline.model.MetadataLoader.mergeOptionSet(MetadataLoader.java:212)
  at io.airlift.airline.model.MetadataLoader$InjectionMetadata.compact(:242)
  at io.airlift.airline.model.MetadataLoader.loadCommand(:81)
  at io.airlift.airline.Cli.<init> ... NodeTool.execute(NodeTool.java:252)
```
`dev` (d6a0542) passes cassandra; any build of `fix-jetty`/`fix-solr2`/`fix-demo`
fails it deterministically. This is a real VM bug — airline's reflection-driven
metadata loader gets a `null` where the real JDK yields an empty collection.
Likely a CratonVM reflection native (`Class.getDeclaredFields`/`getMethods`/
`getDeclaredAnnotations`, or an `@Option`/`@Arguments` field-injection path)
returning `null` instead of an empty array/list, or returning members in an
unstable order. **Fix this first — it unblocks all four branches at once.**

Repro: build any `fix-*` branch worktree, then
`ASCP=$(find apps/apache-cassandra-4.1.4/lib -name '*.jar'|tr '\n' ';'|sed 's/;$//')`
`target/release/java.exe --Xmx 512m -c "$ASCP" org.apache.cassandra.tools.NodeTool version`

## Preserved fix branches — land each AFTER the gate bug is fixed

Each fixes a real bug and is verified for its target app, but is held off `dev`
because it trips the cassandra gate above. Land one at a time, each with a FULL
triage gate (cassandra + tomcat + jetty + JIT-on/off) before the next.

- **`fix-solr2` (`5d2540b`)** — fully fixes Apache Solr: `SolrCLI version` →
  `Solr version is: 9.5.0`, rc=0. Fixes garbled stack traces, the log4j
  `core.Logger.privateConfig` NPE, and removes over-broad `elasticsearch_extras`
  `<clinit>` no-ops. Merges clean into dev.
- **`fix-jetty` (`44c2c78`)** — fixes the Jetty launcher `getClasspath on null`
  NPE via a JIT exception-propagation fix (JIT callers must check for a pending
  exception after every invoke; `invokestatic` path included). Conflicts with
  dev in `native-io/src/lib.rs` (path-confinement — keep BOTH the
  `PATH_CONFINE_TO_CWD` and `SANDBOX_ROOTS` blocks; that resolution is known-good).
- **`fix-demo` (`0f03481`)** — fixes Spring Boot "undersized object layout"
  errors (`read_string` shape-guard; recursion-safe `forEach` for non-ArrayList
  Iterables).
- **`fix-jetty2`** — fixes the Jetty ICU `NormalizerImpl` `StringIndexOOB`.
  Branched from the OLD `fix-jetty` (`660dc6a`); **rebase it onto the final
  `fix-jetty`** before merging.

## Gauntlet loop (orchestrator process)

1. `cargo build --release --bin java`, then `bash scripts/triage.sh` — runs each
   app, captures rc + first error. Logs in `applogs/triage-*`.
2. For each real failure, launch an **opus agent** to investigate + fix ONE bug.
3. Agent isolation: create the worktree YOURSELF — `git worktree add
   .claude/worktrees/fix-X -b fix-X dev` — so it is based on CURRENT dev. Do NOT
   rely on the Agent tool's `isolation:"worktree"` (it branched from a stale base
   last session). Tell the agent its worktree path explicitly.
4. Agents build + verify in their own worktree; you do the integration build.
5. Merge each branch into `dev` only after a full triage gate confirms NO
   regression. Bisect-verify — agents' narrow self-tests miss gauntlet-wide
   regressions (this session: `ClassId(0)`, a tomcat segfault, and the cassandra
   gate all slipped past "agent-verified" branches).

## Pitfalls (learned the hard way this session)

- **`apps/` is `.gitignore`d** — fresh worktrees do NOT contain it. Agents must
  reference apps at the absolute path `C:/craton/CratonVM/apps/...`.
- **CWD path-sandbox artifact** — `native-io`'s path validation confines to the
  process CWD. An agent running `java.exe` from its worktree dir (where `apps/`
  is outside CWD) gets spurious file-access failures (e.g. tomcat rc=1). Run from
  a dir that contains `apps/`, or have the agent account for it.
- **Never bulk-merge recovered/stale branches** — they fork from old bases and
  silently regress `dev` (proven: bulk-merge → `ClassId(0)` broke all 12 apps).
  Integrate only genuinely-new fixes, one at a time, gated by triage.
- Other sessions / the harness autosave commit to `dev`/`main` concurrently —
  re-check `git log` / `git branch --show-current` before committing.
- The JIT-disable env var is `CRATONVM_DISABLE_JIT=1` (use to tell JIT bugs from
  interpreter bugs).

## App status snapshot (on `dev` d6a0542, real bytecode, JIT default)

| App | State |
|---|---|
| tomcat | boots fully (Server startup in N ms) |
| kafka | runs real bytecode, prints real USAGE |
| cassandra / activemq / keycloak26 | pass (start cleanly) |
| solr | FIXED on `fix-solr2` (version 9.5.0) — pending the gate bug |
| jetty | launcher NPE fixed on `fix-jetty`; ICU bug fixed on `fix-jetty2` |
| demo / insurance / felix | Spring undersized-object errors (fix on `fix-demo`) |
| wildfly / keycloak16 | boot into `org.jboss.as.server.Main`; next blocker is
  staxmapper `XMLMapperImpl.parseDocument` NPE (CratonVM `XMLStreamReader` is
  incomplete — `getLocation()/nextTag()/require()/getName()`) |
