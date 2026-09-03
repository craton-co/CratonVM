# H8-1 — three declines that were not declines

**Status: FIXED-UNVERIFIED — no binary carrying these changes has been built or
run.** Every "after" number below is labelled **PREDICTED** and carries its own
falsifier. Nothing here was measured on a running VM.

**Date** 2026-08-20
**Lane** H8 (`--jdk-only` completion, wave H)
**Subject** `native-io/src/**` — the three defects `H5-1` located and did not
have time to fix (`H5-1` §2.B ×2, §3.5, i.e. nominations N2/N3/N4)
**Worktree** `C:/craton/cratonvm/.claude/worktrees/agent-a7561f066bff72e11`
**Base** cut at `26e4b5db4`, fast-forwarded to
`claude/jdk-only-mode-handoff-09b48c` = `db71dfb40` before any edit. The gap
contained `H5-1`, `H2-1` and `HANDOFF-20260820.md`; §7 says what of it touches
this lane.

**Commits**

| SHA | Defect |
|---|---|
| `f83c24f68` | H8-A — delete the dead `sun/nio/ch/UnixDispatcher.close0` registration |
| `b2dbf77e9` | H8-B — mirror the synthetic-RAF gate onto `getFilePointer()J` |
| `e9f08d42b` | H8-C — `native_scanner_close` yields instead of swallowing |

**Instrument** `javap -p -s` against
`C:/Program Files/Microsoft/jdk-25.0.3.9-hotspot` (JDK 25.0.3+9 — **not** the
`Eclipse Adoptium` path 66 records cite, `H5-1` §6.1), plus source reading of
the two dispatch guards in `vm/src/runtime/interpreter/invoke.rs` and
`vm/src/vm/vm_exec.rs`.

> **Line numbers.** All three fixes add long comment blocks, so every number in
> this record has already shifted once. Grep the literal.

---

## 0. What moves, in one table

**None of the three moves either mode.** That is the honest total, and it is
stated first so the rest of the record cannot be read as three mode-moving
fixes.

| Change | Strict (`--jdk-only`) | Compatible | Default-arm behaviour | What it actually buys |
|---|---|---|---|---|
| H8-A | **neither** | **neither** | **unchanged** | deletes a callback that never ran and a ~10-line comment describing what it "does"; removes one superseded row from `--dump-native-registry` |
| H8-B | **neither** | **neither** | **unchanged** | `CRATONVM_SYNTHETIC_RAF=1` can now reach `getFilePointer()J`, which answered a constant `0` in that arm |
| H8-C | **neither** | **neither** | **unchanged** | a decline that completed the call now yields to the receiver's bytecode |

Per `HANDOFF-20260820.md` §1, only retiring a `Bridge`-tagged shadow moves
strict mode. **No registration in this lane changed its `NativeKind`, and only
one registration was deleted — a superseded one, whose triple survives under
another owner.** So the shadow census printed on every strict arm should be
byte-identical. If it is not, something in §5 is wrong.

---

## 1. H8-A — `sun/nio/ch/UnixDispatcher.close0`: a callback that never ran

### 1.1 What was wrong

`native-io/src/lib.rs`, inside `register_io_natives`, registered
`native_fd_close0` for
`sun/nio/ch/UnixDispatcher.close0(Ljava/io/FileDescriptor;)V`, under a comment
that described a `java.net.MulticastSocket` `UnsatisfiedLinkError` fix.

`native-io/src/net.rs:4179` (`register_sun_nio_ch_net`) registers the **same
triple** with a **different callback**, `net_close`. `register()` is
last-write-wins, and `net.rs` runs later in the same boot:

```console
$ grep -rn "register_sun_nio_ch_net" --include=*.rs .
native-io/src/net.rs:4034:pub fn register_sun_nio_ch_net(r: &mut NativeMethodRegistry) {
native-io/src/nio_native.rs:1907:    crate::net::register_sun_nio_ch_net(r);
```

— one call site, and it is the **last statement** of
`register_t16_channel_overrides`, which `register_io_natives` calls hundreds of
lines *after* the deleted registration (lib.rs, "T16.5 / T16.6" block). So
`net_close` has always owned the slot. Independently measured 2026-08-17 with
`--dump-native-registry --jdk-only`: `owns_slot: false`.

### 1.2 Which one is right — and they are NOT behaviourally identical

This is the part the brief asked to decide and justify, and the answer is not
"either".

| | `native_fd_close0` (lib.rs) | `net_close` (net.rs) |
|---|---|---|
| closes | `ctx.fd_table()` entry (`flush` then `close`) | `net_sockets()` entry (`close_net_fd`) |
| clears `fd`/`handle` | yes | yes (`clear_fields: true`) |
| id space | `FileDescriptorTable` counter | `next_net_fd()`, starting at `0x4000_0000` |

`native-io/src/net.rs:827`:

```rust
fn next_net_fd() -> i32 {
    // Start high so there's no collision with the `FileDescriptorTable`
    // counter. ...
    static NEXT: AtomicI32 = AtomicI32::new(0x4000_0000);
```

**The two id spaces are disjoint by construction, and the source says so.** A
`sun.nio.ch` socket descriptor carries a `net_sockets()` id. Had the deleted
registration ever won, `FdTable::flush`/`close` would have missed on every such
id — and `FdTable::close` answers `Ok(())` for anything it does not recognise —
so the OS socket would have stayed open while `fd`/`handle` were set to `-1`.
**A leak that reports success.** `net_close` is also the sibling of the
`preClose0` registration ten lines below it in `net.rs`, which closes the same
registry; the two agree, which the deleted pair did not.

So: **delete, do not reorder.** The previous comment's stated reason for
keeping the dead line — *"it is the only fallback if the
`register_sun_nio_ch_net` call is ever gated"* — proposed a fallback that leaks
the socket, and is recorded in §6 as an in-tree comment that is wrong.

### 1.3 What changes and what does not

- **Behaviour: nothing, in any mode.** The slot already held `net_close`.
- **`native_fd_close0` itself: unchanged and still live**, serving
  `java/io/FileDescriptor.close0()V` (`owns_slot: true`), which is the
  file-stream route the real `FileInputStream.close()` /
  `FileOutputStream.close()` bytecode takes.
- **Strict mode: zero.** Both registrations were `Bridge`; the triple survives
  with the same kind under `net.rs`.
- **Compatible mode: zero**, same reason.
- **The only checkable delta** is that the superseded row disappears from
  `--dump-native-registry`'s registration list.

**Do not let this read as a bug fix.** It is a dead-code and wrong-comment fix.
`G85-1` recorded six retags that passed everything and were inert; this is the
same genus, and it is being labelled as such up front rather than after a green
arm.

### 1.4 What is still OPEN

`H5-1` §10.N3's actual question is untouched: **does
`java.net.MulticastSocket.close()` release its fd under `net_close`?** The
comment that claimed the `native_fd_close0` route served that path was wrong,
but nothing here shows what does serve it. Probe unchanged: open + close a
multicast socket, check the fd table. A source read cannot answer it.

---

## 2. H8-B — the RAF gate that could not reach `getFilePointer()J`

### 2.1 What was wrong

`register_io_extras_natives` (`native-io/src/lib.rs:15415`) registers **18**
synthetic `java/io/RandomAccessFile` triples inside `if !real_raf_enabled()`.
`register_random_access_file_natives`
(`native-io/src/random_access_file.rs:560`) is called **six lines later** in
`register_io_natives` and registered its **10** bridges unconditionally. Last
write wins, so any triple on both lists was served by the bridge in **both**
settings of the flag.

### 2.2 The full list of RAF methods that escape the gate

The brief asks for the whole list, whether or not it changes. It is a
one-element list, and the reason it is one element is structural rather than
lucky.

**Gated block, 18 triples** (`lib.rs`, the `if !real_raf_enabled()` body):

`<init>(Ljava/lang/String;Ljava/lang/String;)V`,
`<init>(Ljava/io/File;Ljava/lang/String;)V`, `read()I`, `read([BII)I`,
`write(I)V`, `write([BII)V`, `seek(J)V`, **`getFilePointer()J`**, `length()J`,
`close()V`, `readInt()I`, `readLong()J`, `writeInt(I)V`, `writeLong(J)V`,
`readFully([B)V`, `readLine()Ljava/lang/String;`,
`readUTF()Ljava/lang/String;`, `writeUTF(Ljava/lang/String;)V`.

**Unconditional registrar, 10 triples** (`random_access_file.rs`):

`initIDs()V`, `open0(Ljava/lang/String;I)V`, `read0()I`, `readBytes0([BII)I`,
`write0(I)V`, `writeBytes0([BII)V`, **`getFilePointer()J`**, `seek0(J)V`,
`length0()J`, `setLength0(J)V`.

**Intersection: `{ getFilePointer()J }`. Exactly one, and no other.**

Why it is the only one:

```console
$ javap -p -s java.io.RandomAccessFile | grep -A1 native
  private native void open0(java.lang.String, int) ...
  private native int read0() ...
  private native int readBytes0(byte[], int, int) ...
  private native void write0(int) ...
  private native void writeBytes0(byte[], int, int) ...
  public native long getFilePointer() throws java.io.IOException;
    descriptor: ()J
  private native void seek0(long) ...
  private native long length0() ...
  private native void setLength0(long) ...
  private static native void initIDs();
```

`getFilePointer()J` is the **only** `public` ACC_NATIVE method on JDK 25's
`RandomAccessFile`. The gated block re-implements the *public* surface; the
registrar bridges the *native* surface; those two sets intersect in exactly the
methods that are both, and there is one.

**Checked the rest of the tree the same way** (the brief's "which others did"):

```console
$ grep -rn '"java/io/RandomAccessFile"' --include=*.rs . | grep -v tests
native-api/src/capability.rs:1337        (a capability table, not a registration)
native-builtins/src/phases_early.rs:2873 (a class-name list, not a registration)
native-builtins/src/phases_late/nio_file.rs:12667
native-builtins/src/shared_secrets_bridge.rs:2154,2166 (ctx.invoke, not register)
native-io/src/lib.rs:15380
```

`native-builtins`' `register_phase57_random_access_file` is the second synthetic
RAF implementation, and **it is correctly gated** on the same condition
(`nio_file.rs:32`: `if crate::vmflags().io.synthetic_raf_forced`), with a
comment that says *"Both crates must skip together"*. It is not part of this
defect. **No other RAF triple escapes any gate.**

### 2.3 What the escape actually cost

Not nothing, and not a mere tag. The two bodies read **different layouts**:

- `native_getFilePointer` (`random_access_file.rs`) → `read_fd` → `this.fd` as
  a **`FileDescriptor` object**, then its `fd`/`handle`.
- `native_raf_get_file_pointer` (`lib.rs`) → `ctx.get_field(this,
  RAF_FIELD_FD)` — an **`Int` in slot 0**, which is what the synthetic
  `native_raf_init` writes. It writes **no `FileDescriptor` at all**.

So under `CRATONVM_SYNTHETIC_RAF=1`, `raf_fd_object` answered `None`,
`read_fd` answered `None`, and `native_getFilePointer` returned its
`Ok(Some(Value::Long(0)))` fallback — **`getFilePointer()` was a constant 0 for
every `RandomAccessFile` in that arm**, while `seek`, `length` and `read` all
worked off the synthetic layout. This is `[flag != mode drops it]` and
`[a never-honoured flag hides every defect behind it]`: nothing behind that
flag has ever been tested for this method.

**This is derived, not measured.** See §5.3 for the falsifier.

### 2.4 The fix, and what does not change

Gate the bridge row on `crate::real_raf_enabled()`
(`random_access_file.rs:617`), rather than un-gating the synthetic twin —
because `getFilePointer` genuinely is ACC_NATIVE, so in the **default**
(real-RAF) arm the bridge is the only implementation that exists and must stay.

- **Default arm (`CRATONVM_SYNTHETIC_RAF` unset): byte-for-byte unchanged.**
  Same callback, same `NativeKind::Bridge`, same site. `real_raf_enabled()` is
  `!io_flags().synthetic_raf_forced`, so the condition is `true` and the row
  registers exactly as before.
- **`CRATONVM_SYNTHETIC_RAF=1` arm:** the slot now holds
  `native_raf_get_file_pointer` at `lib.rs`, reading the layout the synthetic
  `<init>` actually wrote.
- **Both arms register the triple, both as ambient/explicit `Bridge`**
  (`register_io_extras_natives` opens with `set_category(Bridge)`), so no
  census row appears or disappears in either mode.
- `vm/tests/synthetic_diff.rs::real_raf_path` runs `run_class_env("RealRaf",
  &[])` with no overrides and `env_remove`s `CRATONVM_SYNTHETIC_RAF`, i.e. the
  **default** arm. It is unaffected.

---

## 3. H8-C — `native_scanner_close`: `Ok(None)` is not a decline

### 3.1 What was wrong

`native_scanner_close` (`lib.rs:5804`) is registered on three triples:
`java/util/Scanner.close()V`, and — at the foot of `register_scanner_natives`
(`lib.rs:7976-7982`) — `java/io/Closeable.close()V` and
`java/lang/AutoCloseable.close()V`, all `Bridge`.

Its receiver guard declined a non-`Scanner` receiver with `return Ok(None)`.
**`Ok(None)` is a completed void call.** The registry has already taken the
call away from the bytecode; returning it means "close() ran and did nothing".
Any receiver reaching this native through the bare interface therefore had its
`close()` swallowed: nothing ran, nothing threw, the resource stayed open.
`[decline masks]`.

### 3.2 What the correct decline is in this codebase

`MethodCallResult` is `Result<Option<Value>, MethodCallFailed>` and
`MethodCallFailed` has two variants (`InternalError`, `ExceptionThrown`) — so
`H5-1` §3.5 is right that **the result type carries no "fall through" value**.
The yield therefore has to be *performed*, not signalled, and the registry does
provide the primitive:

`NativeContext::invoke_virtual_bytecode_only`
(`native-api/src/registry.rs:2212`; VM impl `vm/src/vm/vm_exec.rs:11044`). Its
own doc states the purpose: a native that must distinguish a real receiver from
a synthetic one calls it "instead of recursing back into itself via
`invoke_virtual` (which would hit the same native registration again and loop
forever)". The VM impl calls `interpreter::execute` directly, deliberately
bypassing `invoke_on_class_shared`'s second, unconditional native check.

There is already a precedent **inside this crate**:
`native-io/src/direct_buffer.rs:1857+`, whose wide `ByteBuffer` accessors bail
to the class-file body through exactly this call, under a comment that says
"The bail is a real virtual dispatch to the class-file body, so it can never
re-enter this native."

(`native-builtins` has a wrapper, `delegate_to_real_bytecode`
(`native-builtins/src/lib.rs:7005`), which additionally distinguishes a
`super.m()` caller so a superclass-registered native does not loop through a
subclass override. It is `pub(crate)` to that crate, and the super-call shape
does not arise here: `AutoCloseable.close` is abstract, so no
`invokespecial AutoCloseable.close()` exists to arrive as a super-call.)

### 3.3 The fix

```rust
let class_id = ctx.class_id_of_object(this);
let class_name = ctx.class_name_arc_of_id(class_id);
if class_name.as_deref() != Some("java/util/Scanner") {
    let Some(name) = class_name else { return Ok(None) };
    if ctx.is_interface_class(class_id) || !ctx.method_exists(&name, "close", "()V") {
        return Ok(None);
    }
    return ctx.invoke_virtual_bytecode_only(this, "close", "()V", &[]);
}
```

Two receivers keep `Ok(None)`, because for them there is nothing to yield **to**
and delegating would replace a silent no-op with a hard failure:

- an **interface-typed** receiver — a fabricated object stamped
  `java/io/Closeable` or `java/lang/AutoCloseable` itself. Its only `close()V`
  is the abstract declaration, and executing that is `[abstract recv]`, not a
  close.
- a receiver whose class resolves **no `close()V` at all**, which would raise
  `NoSuchMethodError` out of a `close()`.

The null-receiver early return at the top of the function is untouched, so
`vm/src/vm/tests.rs::auto_closeable_close_p70` — which calls this triple with
`Value::Object(None)` and asserts it succeeds — is unaffected.

### 3.4 Which receivers reach this path today, and what happens to them

The brief asks this explicitly, and the honest answer is **as far as a source
read can establish: none**. A resource leak that reports success is worse than
a crash, so the population matters more than the patch.

1. **Through `java/util/Scanner.close()V`** — `java.util.Scanner` is `final`,
   so a receiver whose declaring class resolves there *is* a `Scanner`, and the
   guard's `!=` arm is not taken. Unchanged.
2. **Through the two interface doors** — those doors do not open. Both dispatch
   guards skip a native whose declaring class is an interface and whose method
   is an instance method:
   - `vm/src/runtime/interpreter/invoke.rs:4189` (step 6):
     `if (!(declaring_is_interface && !is_static) || force_interface_default_native)`
   - `vm/src/vm/vm_exec.rs:27907`: `let override_cb = if declaring_is_interface
     && !is_static && !force_… { None }`

   The escape hatch in both is
   `should_force_registered_native_over_bytecode` (plus the named FFM
   exemptions). Neither interface is in it:

   ```console
   $ grep -rn '"java/io/Closeable"\|"java/lang/AutoCloseable"' --include=*.rs .
   classloading/src/class_manager.rs:4421,4423,11483-11491   (interface hierarchy tables)
   classloading/src/vtype.rs:548,561                          (assignability)
   native-io/src/lib.rs:7976,7978                             (THESE registrations)
   vm/src/vm/tests.rs:53154, vm/tests/io_bootstrap_tests.rs:430
   ```

   That grep also establishes something stronger and worth stating on its own:
   **`native_scanner_close` is the only registration of
   `java/io/Closeable.close()V` or `java/lang/AutoCloseable.close()V` anywhere
   in the workspace.** There is no rival.

   Further, `invoke.rs` step 5 bails on abstract methods before step 6 is
   reached, so a receiver that resolves only the abstract declaration never
   arrives here either.

**After the fix:** the same population — none — plus a correct answer for the
two ways the population could become non-empty: a future force-route of either
interface, or a fabricated receiver stamped `java/util/Scanner`. In both cases
`close()` now means close.

**So H8-C makes a backstop correct rather than repairing an observed leak, and
it moves neither mode.** Stated plainly, per the brief's warning about
inert changes reading as fixes.

### 3.5 The residual this leaves

`H5-1` §10.N2 offered two dispositions: *"Either the guard needs a
yield-to-bytecode mechanism, or the two interface registrations need to go."*
This lane did the first. **The second is now better supported than it was**:
the doors are dead, so the two registrations are dead weight on two of the most
implemented interfaces in the JDK. Deleting them is a separate, checkable
change with its own census effect, and it is nominated (§9 N2) rather than
smuggled in here.

---

## 4. Where the tree is WRONG about itself

Each backed by a grep or a `javap` line. H8-A is itself an instance, so the
comments near it were checked too.

**4.1 The deleted `UnixDispatcher.close0` comment claimed a fallback that
leaks.** *"Left in place rather than deleted: … it is the only fallback if the
`register_sun_nio_ch_net` call is ever gated."* `native_fd_close0` closes an
`fd_table` id; `next_net_fd()` (net.rs:827) starts at `0x4000_0000` "so there's
no collision with the `FileDescriptorTable` counter". The proposed fallback
cannot find the socket. §1.2.

**4.2 The `native_fd_close0` body's cross-reference (lib.rs ~2080) cited a
registration that no longer exists.** It said the dump "reports the
`sun/nio/ch/UnixDispatcher.close0` registration made twenty lines below this
function's own registration". True when written; H8-A deleted it. **Corrected
in `f83c24f68`** — the same commit, so the tree is never inconsistent.

**4.3 The RAF gate comment named a RETIRED environment variable.** It said
*"DIAGNOSTIC GATE (CRATONVM_REAL_RAF=1)"*.

```console
$ grep -rn "REAL_RAF" --include=*.rs . | grep -v "^./native-io"
types/tests/flag_declaration_guard.rs:134   "CRATONVM_REAL_RAF", "kind 1: a retired gate…"
vm/tests/synthetic_diff.rs:300,537
$ grep -n "off_key" types/src/flag_groups.rs | grep raf
1357: E { group: Group::REAL, token: "raf", …, off_key: Some("CRATONVM_SYNTHETIC_RAF"), … }
```

Nothing under any `src/` reads `CRATONVM_REAL_RAF`. Setting it does nothing.
**Corrected in `b2dbf77e9`.**

**4.4 The same comment had the gate's polarity BACKWARDS.** It ended *"Default
(unset) = synthetic."* `real_raf_enabled()` is
`!io_flags().synthetic_raf_forced` (lib.rs), so the default is **real**-RAF and
the block is skipped unless the operator opts in. The doc comment eight lines
above it has said so since 2026-06-02; these lines were never updated with it.
Two adjacent comments in one function contradicting each other. **Corrected.**

**4.5 The same comment's stated reason for gating was fixed three months
ago.** *"gated rather than removed because the real ctor's
FileCleanable/Cleaner/PhantomReference path still has a separate crash under
sustained load that is being diagnosed"* — the `real_raf_enabled` doc directly
above says that SEGV/Cleaner crash is FIXED
(`app-jvm-bugs/real-raf-segv-root-cause.md`, 2026-06-02). **Corrected, without
inventing a replacement reason** — I do not know why the block is kept beyond
"it is an opt-in escape hatch", and the comment now says so rather than
guessing.

**4.6 `random_access_file.rs`'s module doc asserted the exact false premise
that produced H8-B.** *"The two systems never collide because they register
different method names."* They collide on `getFilePointer()J`. **Corrected.**

**4.7 `types/tests/flag_declaration_guard.rs:134-138`'s justification for its
`CRATONVM_REAL_RAF` row is factually wrong, and was wrong before this lane.**
It says *"the only surviving mention is the `env_remove` baseline list in
`vm/tests/synthetic_diff.rs`"*. `native-io/src/lib.rs` mentioned it too (in a
comment, which `is_comment_line` drops, so no test was red), and
`vm/tests/synthetic_diff.rs:537`'s own doc comment mentions it a second time.
**Not corrected — out-of-file.** §8.

**4.8 `H5-1` §2.B is right, and its two "not deliberate" rows are the two this
lane fixed.** Re-verified independently, not taken on trust: the ordering claim
via `grep -rn register_sun_nio_ch_net` (one call site, last statement of
`register_t16_channel_overrides`), the RAF claim via the two registrar bodies
and `javap`. No correction to `H5-1` is needed.

---

> **VERIFIED AGAINST A BINARY 2026-09-02.** §5 opened "Nothing below has been
> run." §5.1's three rows were checked against `--dump-native-registry` on both
> arms, from a build of this tree. **Two hold; the third was superseded by a
> later, deliberate retirement — not falsified.**
>
> ```text
>                                        --real-jdk and --jdk-only, identical
> sun/nio/ch/UnixDispatcher.close0       kind=bridge  native-io/src/net.rs:4274
> java/io/RandomAccessFile.getFilePointer kind=bridge  native-io/src/random_access_file.rs:624
> java/io/Closeable.close                ABSENT
> java/lang/AutoCloseable.close          ABSENT
> ```
>
> * **H8-A's falsifier did not fire.** The surviving `close0` row names
>   `net.rs`/`net_close`, exactly as §1.1's ordering argument requires, and the
>   superseded `lib.rs` entry is gone. That was "the whole visible effect of
>   H8-A".
> * **H8-B's falsifier did not fire.** `getFilePointer` is PRESENT in the default
>   arm. §5.1 says its absence would mean `real_raf_enabled()` is inverted and
>   "revert immediately, that would break real-JDK RAF entirely". It is present.
> * **The `Closeable` / `AutoCloseable` rows are absent, and §5.1 predicted
>   "present, unchanged".** This is NOT a regression against H8-C: the two
>   registrations were **RETIRED on 2026-08-21 by WORKER 4**, the day after this
>   record, and the deleted lines are preserved verbatim in a comment at
>   `native-io/src/lib.rs:8902`. The reason given is this campaign's
>   interface-registration family: dispatch keys on the RECEIVER, so an interface
>   instance-method row is shut out at step 1 of `execute_invoke_kind`, and again
>   at step 6's interface-default gate. They served nobody. `H11-3` N1 had
>   written the deletion out verbatim and could not make it because `vm/` was out
>   of that lane's bounds.
>
> So the third row is stale by a documented, intentional change with a named
> owner and a preserved diff — the distinction worth drawing, because "predicted
> present, measured absent" reads as a falsifier until you look at why.
>
> **NOT verified, and §5.3 says so itself:** H8-B's behaviour change lives only
> in the `CRATONVM_SYNTHETIC_RAF=1` arm, which no regression vector sets. A green
> suite is explicitly not evidence for it, and this note did not set that flag.

## 5. VERIFICATION PLAN

Nothing below has been run. Build the three arms and diff.

### 5.1 The registry dumps

`--dump-native-registry`, `--real-jdk` and `--jdk-only`:

| Row | PREDICTED after |
|---|---|
| `sun/nio/ch/UnixDispatcher.close0(Ljava/io/FileDescriptor;)V` | **present, unchanged**: `kind: bridge`, callback `net_close`, site `native-io/src/net.rs`. The **superseded** entry naming `native-io/src/lib.rs` is **gone** — that is the whole visible effect of H8-A. |
| `java/io/RandomAccessFile.getFilePointer()J` | **present, unchanged** in the default arm: `kind: bridge`, site `native-io/src/random_access_file.rs`. |
| `java/io/Closeable.close()V`, `java/lang/AutoCloseable.close()V` | **present, unchanged** (`bridge`, `native-io/src/lib.rs`). H8-C changed a body, not a registration. |
| every other row | **unchanged** |

**Falsifier for H8-A:** if the surviving `UnixDispatcher.close0` row names
anything other than `net.rs`/`net_close`, the ordering argument in §1.1 is
wrong and the delete changed behaviour.
**Falsifier for H8-B:** if `getFilePointer` is **absent** from a default-arm
dump, `real_raf_enabled()` is not what §2.4 says it is and the gate is
inverted — revert immediately, that would break real-JDK RAF entirely.

### 5.2 The strict-mode report

`cratonvm --jdk-only --jdk-only-report r.json`, union across the `--jdk-only`
corpus, plus the shadow census now printed on every strict arm.

- **PREDICTED: the census is IDENTICAL to the pre-lane one.** No triple
  appears, disappears, or changes `kind`.
- **Falsifier:** any delta at all. This lane claims zero mode movement; a
  single row's difference means one of §1.3, §2.4 or §3.4 is wrong about what
  it touched.

This is the cheapest falsifier in the record and it should be checked first.

### 5.3 The one behavioural probe, and it needs a flag the corpus does not set

H8-B's behaviour change lives **only** in the `CRATONVM_SYNTHETIC_RAF=1` arm,
which no regression vector sets. It therefore cannot be confirmed by the 102-
or 104-vector arm at all, and a green arm is **not** evidence for it.

```java
// probes/RafPointer.java
import java.io.*;
public class RafPointer {
  public static void main(String[] a) throws Exception {
    File f = File.createTempFile("rafp", ".bin"); f.deleteOnExit();
    try (RandomAccessFile r = new RandomAccessFile(f, "rw")) {
      r.writeInt(0xdeadbeef); r.writeLong(42L);
      System.out.println("afterWrite=" + r.getFilePointer());  // expect 12
      r.seek(4);
      System.out.println("afterSeek=" + r.getFilePointer());   // expect 4
    }
  }
}
```

| Arm | Before (PREDICTED) | After (PREDICTED) |
|---|---|---|
| default (flag unset) | `afterWrite=12 afterSeek=4` | **identical** |
| `CRATONVM_SYNTHETIC_RAF=1` | `afterWrite=0 afterSeek=0` | `afterWrite=12 afterSeek=4` |

**Falsifier:** if the *default* arm moves at all, the gate condition is wrong.
If the synthetic arm still prints `0`, then either the synthetic
`native_raf_get_file_pointer` reads a slot the synthetic `<init>` does not
write, or a third registrar owns the triple — and §2.2's intersection is
incomplete.

Run it against HotSpot too (`HS=oracle`): `12` / `4` is the oracle answer, not
a guess.

### 5.4 Vectors

Full arm, not the 36-vector screen (`G90-1` §5).

| Vector | Expectation | Why |
|---|---|---|
| `RFileTimes` | **must stay green** — see the attribution warning below | file-stream open/write/close plus `lastModified` |
| `RNioNoFollow`, `RFsSingleton` | **must stay green** | `java.nio.file`; untouched |
| `RChannelInterrupt`, `RSocketChannelInterrupt`, `RJdkNet` | **must stay green** | the `sun.nio.ch` / `Net` surface — the only vectors that can see H8-A at all, and only if the delete changed the winner (it must not) |
| `RDataInputFastPull` | **must stay green** | `register_data_stream_natives`; untouched |
| anything closing a resource through an `AutoCloseable`-typed variable | **must stay green** | H8-C's branch; §3.4 predicts it is never entered |

**ATTRIBUTION WARNING, carried forward from `H5-1` §9 and restated because it
now has a third claimant.** `RFileTimes` is exercised by `H2-1`'s FILETIME
encoding change **and** by `H5-1`'s `FileInputStream` tag change **and** sits
downstream of this lane's `native_fd_close0` neighbourhood. **A move in
`RFileTimes` must not be attributed to any one lane without an isolated
build.** `[fix+fix≠]`.

What distinguishes them, cheaply, without three builds:

- **H2-1** changed `native-builtins/src/phases_late/nio_file.rs` and retired
  eight `sun/nio/fs/WindowsFileAttributes` shadows. Its signature is a wrong
  **timestamp value** or a changed `sun/nio/fs/*` census row.
- **H5-1** changed the `kind` of `java/io/FileInputStream.read([BII)I`. Its
  signature is a new or reworded **`IndexOutOfBoundsException`** from a bulk
  read, and a `--dump-native-registry` row that flips `bridge` →
  `synthetic-stub`.
- **H8 (this lane)** changed **no registration's kind and no timestamp path**.
  Its only possible signature in `RFileTimes` is a `close()` that stopped
  releasing an fd — which would require the `UnixDispatcher.close0` winner to
  have changed, which §5.1 checks directly.

So: read the dump first. If the `UnixDispatcher.close0` row still names
`net.rs`, this lane is not the claimant, and the choice is between H2-1 and
H5-1 on the two signatures above.

### 5.5 What a reader should re-derive rather than trust

- **§2.2's intersection.** It is a hand-diff of two lists in two files. Re-run
  the two greps and the `javap` before quoting "exactly one".
- **§3.4's "the doors do not open".** It is a source read of two guards in two
  files, not a run. `H5-1` §10.N1 — *does CratonVM native dispatch key on the
  receiver's class or on the constant-pool class?* — is **still open**, and if
  the answer is "constant-pool class" for some third dispatch site this record
  did not find, the population in §3.4 is not empty and H8-C becomes a real
  fix rather than a backstop. The one-run probe is still N1's: a `Pipe`
  source assigned to an interface-typed local, `--dump-native-registry` read
  for `invocations` on the abstract row.

---

## 6. Files touched

| File | Defect | Nature |
|---|---|---|
| `native-io/src/lib.rs` | H8-A, H8-B, H8-C | one registration deleted; one guard body changed; four comment blocks corrected |
| `native-io/src/random_access_file.rs` | H8-B | one registration gated; module doc corrected |
| `docs/known-issues/jdk-only/H8-1-three-declines-that-were-not-declines-20260820.md` | — | this record |

No file outside `native-io/src/**` and this record was modified.

---

## 7. Interaction with the rest of wave H

| Gap content | Touches this lane? |
|---|---|
| `H5-1` | **Yes — it is the input.** All three defects are its §2.B/§3.5 findings. Its §1 change (`FileInputStream.read([BII)I`) is in a different function of the same file and did not conflict. |
| `H2-1` (`nio_file.rs` FILETIME + eight `sun/nio/fs/` retirements) | **No source overlap.** This lane touches no `sun/nio/fs/*` and no attribute path. But H2-1's `register_phase57_random_access_file` neighbour in `nio_file.rs` was read as part of §2.2 and is correctly gated — H8-B does **not** need a matching edit there. |
| `RFileTimes` overlap | **Yes, for attribution only.** §5.4. |
| `HANDOFF-20260820.md` §1 (`SyntheticStub` is not `allowed_in(JdkOnly)`) | **Yes** — it is why §0 can say "moves neither mode" with confidence: nothing here re-tags anything, so the `Bridge`/`SyntheticStub` distinction never comes into play. |

---

## 8. OUT-OF-FILE EDITS REQUIRED

**None are required for these changes to build or to be correct.** Two are
requested as corrections of record; both are comment/justification text and
neither can turn a gate red.

**8.1 — `types/tests/flag_declaration_guard.rs`, the `CRATONVM_REAL_RAF` row
(currently around line 134).**

Current text:

```rust
    (
        "CRATONVM_REAL_RAF",
        "kind 1: a retired gate. Real-RAF is the default and the opt-out is \
         `CRATONVM_SYNTHETIC_RAF`; the only surviving mention is the \
         `env_remove` baseline list in `vm/tests/synthetic_diff.rs`. Delete \
         this row when that list is trimmed.",
    ),
```

Replacement text:

```rust
    (
        "CRATONVM_REAL_RAF",
        "kind 1: a retired gate. Real-RAF is the default and the opt-out is \
         `CRATONVM_SYNTHETIC_RAF`. The surviving mentions are all in COMMENTS, \
         which `is_comment_line` drops, plus the `env_remove` baseline list in \
         `vm/tests/synthetic_diff.rs` (which is why this row exists at all). \
         An earlier version of this note claimed that list was the ONLY \
         mention; it was not — `native-io/src/lib.rs` named the retired \
         variable in the RAF gate's own comment until H8-1 corrected it. \
         Delete this row when the `env_remove` list is trimmed.",
    ),
```

Reason: the row's justification is the thing the next reader will trust, and it
was wrong. See §4.7.

**8.2 — `vm/tests/synthetic_diff.rs`, the doc comment on `real_raf_path`
(currently around line 536).**

Current text:

```rust
/// Smoke test: RandomAccessFile routes to real JDK bytecode under
/// CRATONVM_REAL_RAF=1.  Writes an int + long, seeks back, reads them, and
```

Replacement text:

```rust
/// Smoke test: RandomAccessFile routes to real JDK bytecode in the DEFAULT
/// arm — this test passes no overrides and `env_remove`s
/// `CRATONVM_SYNTHETIC_RAF`, and real-RAF has been the default since
/// 2026-06-02. (`CRATONVM_REAL_RAF`, which this comment used to name, is a
/// retired gate that nothing under any `src/` reads.)  Writes an int + long,
/// seeks back, reads them, and
```

Reason: the test's name and comment describe a variable that does nothing, so
the test appears to cover the flag and does not. See §4.3.

---

## 9. NOMINATIONS

**N1 — delete the two interface `close()V` registrations.** `H5-1` §10.N2's
other disposition, now better supported: §3.4 shows both dispatch guards skip
natives on interface instance methods and that neither interface is
force-routed, so these two `Bridge` rows on `java/io/Closeable` and
`java/lang/AutoCloseable` — the only registrations of either triple in the
workspace — are dead weight sitting on two of the most-implemented interfaces
in the JDK. It is a **checkable** change (two rows leave the census) and it
should be measured, not assumed: run `--dump-native-registry` before and after
and confirm the delta is exactly two. **Not done here** because this lane's
whole claim is "moves nothing", and this would move something.

**N2 — the RAF layout split is the real defect; the gate is the symptom.**
§2.3 shows the synthetic RAF stores an `Int` fd in slot 0 while the bridge
half reads a `FileDescriptor` object, and the `real_raf_enabled` doc already
calls the synthetic path *broken*, with a second broken copy in
`native-builtins`. Two implementations of one class with incompatible layouts,
kept alive behind an opt-in nobody exercises, is three landmines in a trench
(`[flag=3 landmines]`). Either retire the synthetic RAF path outright or make
it write a real `FileDescriptor`. `getFilePointer` was the one method where the
mismatch became visible; it is unlikely to be the only one where it exists.

**N3 — H5-1 §10.N3 is still open and this lane could not close it.** Does
`java.net.MulticastSocket.close()` release its fd under `net_close`? §1.4. One
run answers it.

**N4 — the `preClose0` twin deserves the same read as `close0`.**
`net.rs:4189` registers `sun/nio/ch/UnixDispatcher.preClose0` with an inline
closure calling `close_net_fd_descriptor(ctx, fd_obj, false)`, and
`nio_native.rs:1240` registers a `close0`/`preClose0` pair on the
`FileDispatcherImpl` names with a **different** `native_fd_close0` (the
`nio_native.rs` one, not `lib.rs`'s). Two same-named functions in two modules
of one crate, both bound to `close0`-shaped triples, is the shape that produced
H8-A. Nothing was found wrong there — the classes are genuinely different — but
the naming is a trap and one of the two should be renamed.

**N5 — `java/io/FileOutputStream` still has the shape `H5-1` §10.N5 named**, and
this lane read enough of `native_fd_close0` to agree: the `FileOutputStream`
public surface is 8 `Bridge` shadows with no `SyntheticStub` block, where
`FileInputStream` now has one. Unchanged by this lane; re-nominated so it does
not fall off the list.
