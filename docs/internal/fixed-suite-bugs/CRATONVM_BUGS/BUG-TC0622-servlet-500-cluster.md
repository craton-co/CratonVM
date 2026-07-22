# Bug TC0622 — servlet 500 cluster (`expected:<200> but was:<500>`): one new non-JSP defect + four folding into the existing JSP bug

> **One-line summary:** Of the five `expected:<200> but was:<500>` failures in
> this cluster, **four are the already-filed JSP runtime-compile/load defect**
> (`BUG-TC0622-jsp-pagecontext-contains-npe.md`) and only **one is new**:
> `TestCoyoteOutputStream.testWriteWithByteBuffer` 500s because the server-side
> servlet does `new RandomAccessFile(file,"r").getChannel().map(READ_ONLY, …)`,
> and **CratonVM's `RandomAccessFile` and `FileChannel` use two disjoint
> file-handle registries.** The RAF handle (≥ 1000, stored in `fd.fd`/`fd.handle`)
> is never registered in the nio `fd_table`, so `FileChannel.size()` →
> `FileDispatcherImpl.size0(FileDescriptor)` → `fd_table().file_size(fd)` cannot
> find the fd and throws `java/io/IOException: size0: bad fd for size`. HotSpot
> shares one FD between the RAF and its channel, so `getChannel().map()` works
> and the request returns 200.

**Severity:** Medium (breaks any servlet/code path that memory-maps or otherwise
uses `FileChannel` derived from a `RandomAccessFile`).
**Status on CratonVM:** FAIL. **HotSpot:** PASS.
**Run date:** 2026-06-22
**Binary:** dev `df11ac00` (worktree `C:\craton\CratonVM-tctest`).

## Cluster triage — five classes, two root causes

All five fail client-side identically with `java.lang.AssertionError: expected:<200>
but was:<500>` — i.e. an HTTP request returned a server-side-exception 500.
Reading each captured `*.log.err` for the server-side stacktrace between the
request and the assertion splits the cluster cleanly:

### (a) Four classes fold into the existing JSP bug — DO NOT re-document

These reproduce the exact server-side signatures already documented in
`BUG-TC0622-jsp-pagecontext-contains-npe.md` (runtime-JSP-compile/load and the
JSTL `JstlCoreTLV`→`JstlBaseTLV` define failure). They are **additional affected
classes of that bug**, not new defects:

| Class | Server-side root cause in `.log.err` | JSP-bug variant |
|---|---|---|
| `jakarta.el.TestOptionalELResolverInJsp` | `JasperException: Unable to load class for JSP` → `ClassNotFoundException: org.apache.jsp.tag.web.echo_tag` | generated `org.apache.jsp.*` not loadable |
| `jakarta.servlet.jsp.el.TestImportELResolver` | `ClassNotFoundException: org.apache.jsp.bug6nnnn.bug66582_jsp` (and `…bug66441_jsp`) | generated `org.apache.jsp.*` not loadable |
| `jakarta.el.TestCompositeELResolver` | `ClassLoader.defineClass1(org/apache/taglibs/standard/tlv/JstlCoreTLV) failed: ClassNotFound JstlBaseTLV` → `JasperException: Failed to load or instantiate TagLibraryValidator class` → root cause NPE `Class.getPackageName() because "c" is null` | JSTL TLV superclass-resolution variant |
| `jakarta.servlet.TestSessionCookieConfig` | `JasperException: ClassNotFoundException: org.apache.jsp.bug49nnn.bug49196_jsp` | generated `org.apache.jsp.*` not loadable |

> **NOTE on the task hypothesis:** `TestSessionCookieConfig` was expected to be a
> servlet/response-API gap distinct from JSP. It is **not** — its `.log.err`
> shows the identical `ClassNotFoundException: org.apache.jsp.bug49nnn.bug49196_jsp`
> as `TestPageContext` (the JSP bug's primary class). `testCustomAttribute`
> exercises a cookie attribute through a JSP, so it 500s on the same generated-class
> load failure. Likewise `TestCompositeELResolver` is the JSTL-TLV variant. Both
> fold into `BUG-TC0622-jsp-pagecontext-contains-npe.md`; please add the four
> classes above to that doc's "Affected classes" list.

### (b) One new, non-JSP defect — documented below

| Class | Server-side root cause in `.log.err` |
|---|---|
| `org.apache.catalina.connector.TestCoyoteOutputStream` (`testWriteWithByteBuffer`) | `Servlet.service() … threw exception (java/io/IOException: size0: bad fd for size)` |

This is unrelated to JSP and unrelated to DF05 (the regex `new String(StringBuilder)`
cast). It is a `RandomAccessFile`↔`FileChannel` file-descriptor-registry mismatch.

## New defect — symptom

`org.apache.catalina.connector.TestCoyoteOutputStream.log.err`:

```
ERROR [...[/].[testServlet]] Servlet.service() for servlet [testServlet]
  in context with path [] threw exception (java/io/IOException: size0: bad fd for size)
...
java.lang.AssertionError: expected:<200> but was:<500>
  at org.apache.catalina.connector.TestCoyoteOutputStream.testWriteWithByteBuffer(TestCoyoteOutputStream.java:118)
```

The server-side servlet (`TestCoyoteOutputStream.java:269` `TestServlet.doGet`,
line 276-278) memory-maps a file and writes it to the response:

```java
CoyoteOutputStream os = (CoyoteOutputStream) resp.getOutputStream();
File file = new File("test/org/apache/catalina/connector/test_content.txt");
try (RandomAccessFile raf = new RandomAccessFile(file, "r")) {
    os.write(raf.getChannel().map(MapMode.READ_ONLY, 0, file.length()));  // <-- 500 here
}
```

`raf.getChannel().map(READ_ONLY, 0, file.length())` drives the JDK's
`FileChannelImpl.map`, which first calls `nd.size(fd)` (→
`sun/nio/ch/FileDispatcherImpl.size0(FileDescriptor)`) to validate the
mapping bounds. That `size0` is what throws `IOException: size0: bad fd for
size`, so `map` never completes, the servlet's `doGet` propagates the
`IOException`, and Tomcat returns an empty **500**. (`test_content.txt` exists —
910 bytes — so this is not a missing-file issue.)

## New defect — root cause

CratonVM has **two independent, non-overlapping file-handle registries**, and
`RandomAccessFile.getChannel()` straddles them:

1. **`RandomAccessFile` natives** (`native-io/src/random_access_file.rs`) keep
   their own module-local `handle_map` (`NEXT_HANDLE: AtomicI64 = 1000`,
   line 91). `open0` allocates a handle id (≥ 1000) and `write_handle`
   (lines 225-249) stores that **same id into both `fd.fd` (int) and
   `fd.handle` (long)** of the `RandomAccessFile`'s `FileDescriptor`. The open
   file is registered **only** in this private map — never in the nio
   `fd_table`.

2. **nio `FileChannel`/`FileDispatcherImpl` natives**
   (`native-io/src/nio_native.rs`, `file_channel.rs`) resolve a
   `FileDescriptor` to a `FdId` via `fd_from_descriptor`
   (`nio_native.rs:45`), which reads `fd.handle` first then `fd.fd`, and then
   index the **global `fd_table`** (`native-api/src/fd_table.rs`).

When the servlet calls `raf.getChannel()`, the resulting `FileChannel` shares
the RAF's `FileDescriptor` (handle id ≥ 1000). On `map`/`size`, the nio path
runs:

```
native_fd_size0 (nio_native.rs:225)
  fd = fd_from_descriptor(fd_obj)          // reads fd.handle = e.g. 1003
  ctx.fd_table().file_size(1003)           // 1003 is NOT in fd_table
```

`FdTable::file_size` (`fd_table.rs:879`) then fails one of two ways, both of
which surface as the same message:

- the id is absent → `get_entry(1003)` returns `None` →
  `Err(NotFound, "bad fd for size")` (line 883); or
- the id collides with an unrelated `fd_table` entry of another variant
  (sockets/pipes; `fd_table` allocates from `next_fd = 3` upward and a busy
  Tomcat process can reach 1000+) → the `match` falls through to the
  `_ => Err(NotFound, "bad fd for size")` arm (line 900).

`native_fd_size0` wraps that as `io_error(format!("size0: {e}"))` →
`size0: bad fd for size`. **The same disjoint-registry flaw breaks the whole
RAF-derived channel path:** had `size0` passed, `native_fc_map0`
(`file_channel.rs:194-203`) would next call `fd_table().clone_file(fd)` and
fail identically with `map0: clone fd: bad fd`. `random_access_file.rs` has **no
`getChannel` bridge** that re-registers the RAF file into `fd_table`, so any nio
operation on a RAF channel is unreachable.

HotSpot has a single FD per `RandomAccessFile`/`FileChannel` pair, so
`size0`/`map0` see a valid descriptor and the request returns 200.

## Reproduction

Suite was running concurrently; the captured `.log.err` above is authoritative.
To reproduce in isolation:

```powershell
cd C:\craton\CratonVM\apps\tomcat
$exe = "C:\craton\CratonVM-tctest\target\release\cratonvm-tcfull-0622.exe"
$cp  = (Get-Content .tooling\cp.txt -Raw).Trim()
$env:CRATONVM_REAL_NET_SOCKETS=1; $env:CRATONVM_REAL_AQS=1
$env:CRATONVM_DISABLE_DEFAULT_WATCHDOG=1
& $exe -Xmx2g -cp $cp org.junit.runner.JUnitCore `
    org.apache.catalina.connector.TestCoyoteOutputStream
# Expect: testWriteWithByteBuffer FAILs expected:<200> but was:<500>,
# server log: IOException: size0: bad fd for size
```

Minimal server-free repro (no Tomcat needed) — exercises the same two-registry
mismatch directly:

```java
java.io.File f = new java.io.File("test_content.txt"); // any existing file
try (java.io.RandomAccessFile raf = new java.io.RandomAccessFile(f, "r")) {
    java.nio.channels.FileChannel ch = raf.getChannel();
    System.out.println(ch.size());                       // throws IOException: size0: bad fd for size
    // ch.map(FileChannel.MapMode.READ_ONLY, 0, f.length()); // same failure
}
```

## Recommendation — FIX (not handoff)

The bug is well localized and the fix is bounded VM-side; no test/source edits.
Bridge the two registries so a `RandomAccessFile`-derived `FileChannel` resolves
to a real `fd_table` entry. Options, cheapest first:

1. **Register RAF opens in `fd_table` and store that fd in the descriptor.**
   In `random_access_file.rs::native_open0`, instead of (or in addition to) the
   module-local `handle_map`, open via `ctx.fd_table().open_read_write(path,
   create)` (`fd_table.rs:672`, which already produces a `FileReadWrite` entry
   that `file_size` handles) and write the returned `FdId` into `fd.fd`/
   `fd.handle`. Then both the RAF read/seek/length natives and the nio channel
   natives resolve the **same** entry. (Care: RAF "r" must not open with
   `write(true)` on a read-only file — add a read-only `open_read` to `fd_table`
   if needed, mirroring `open_read` at line ~280.)

2. **Or add a `getChannel`-time bridge:** when `RandomAccessFile.getChannel()`
   (the `FileChannel` materialization) runs, re-open / dup the file into
   `fd_table` and rewrite the descriptor's fd to the `fd_table` id, so nio ops
   find it. Heavier (two open handles) and risks position/length desync.

Option 1 is preferred: a single registry eliminates the namespace overlap (the
RAF ≥ 1000 vs `fd_table` ≥ 3 ranges currently overlap, which is itself a latent
wrong-file hazard) and removes the duplicate handle-table code in
`random_access_file.rs`. Verify with the minimal repro above and the
`TestCoyoteOutputStream.testWriteWithByteBuffer` E2E (200 + body equals
`test_content.txt`).

## Relation to other docs

- **Folds four classes into** `BUG-TC0622-jsp-pagecontext-contains-npe.md`
  (TestOptionalELResolverInJsp, TestImportELResolver, TestCompositeELResolver,
  TestSessionCookieConfig) — same JSP runtime-compile/load + JSTL-TLV root cause.
- **Not DF05** (`BUG-DF05-...`): that is the regex `new String(StringBuilder)`
  cast; this is an fd-registry mismatch.
- The `getUrl` → 500 → assertion shape is the standard `TomcatBaseTest` pattern;
  the only CratonVM defect here is the server-side `size0` IOException.
