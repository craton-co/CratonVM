# `method-nowhere` deleted 179 registrations, 65 of them live — and 24 tests said so

**Status:** FIXED 2026-08-11. Both crates green: `native-builtins --lib`
3,380/22 → **3,402/0**, `native-io --lib` 436/2 → **438/0**.

Companion to
`fixed-bugs/jdk-only-bridge-on-a-receiver-no-image-declares-FIXED-20260810.md`,
which corrected the *other* half of the same deletion list. That record fixed
the `class-absent` bucket; this one is about the `method-nowhere` bucket both
records agreed to leave as deletion candidates. They were both wrong about it,
in the same way, one bucket apart.

## What happened

`dc55e8057` deleted 179 registrations the dead-sweep scored `method-nowhere` —
the class **is** on a supported image, the method is not. That bucket had
survived two rounds of scrutiny with the same reasoning each time:

> the class IS a real JDK class, so the receiver's existence is not in question,
> only the method's.

True, and it does not license deletion. **A method the JDK does not declare is
exactly what CratonVM's own additions to a real class look like**, and the
census cannot tell those from a dead entry. The commit shipped with
`cargo check --workspace --all-targets` green — which compiles tests and runs
none — so 24 red tests across two crates reached `dev`.

## The three shapes the census could not see

**1. A stand-in for a bytecode method is `method-nowhere` by construction.**
Thirteen of the deletions were in `shared_secrets_bridge.rs`, whose owners are
the JDK's own anonymous `Java*Access` implementations — `java/lang/System$1`,
`java/net/InetAddress$1`, `java/lang/invoke/MethodHandleImpl$1`. Those methods
are ordinary bytecode on every image; CratonVM registers stand-ins so the
shared secret answers something. `ACC_NATIVE` is never true for any of them and
never will be.

`representative_method_registered_per_owner` names one method per owner and is
the statement of that intent. It reports the **first** gap it finds, so
restoring took three rounds — `MethodHandleImpl$1`, then `InetAddress$1`, then
`AccessController$1`/`ZipFile$1` — each run revealing the next. A coverage test
that stops at the first failure hides the size of a regression; that is worth
knowing before reading one as "only one thing broke".

**2. A `<clinit>` no-op shim can never be declared native.**
`java/security/Provider$ServiceKey.<clinit>` scores `method-nowhere` on every
image there will ever be, because `<clinit>` is not a method an image declares
`ACC_NATIVE`. Any sweep keyed on that column will propose deleting every
`<clinit>` shim in the tree, forever.

**3. A deliberate convenience overload is invisible, and its comment says so.**
`jdk/internal/util/Preconditions.checkIndex(II)I` — the JDK only declares the
`BiFunction`-taking form. Its own coverage test explains the bare shapes in a
comment written before any of this:

> A missing entry hands that method to bytecode, which is correct but costs a
> Java frame on `String.charAt`'s per-character path — the only reason any of
> this is native.

So deleting it is a silent per-character regression on the hottest string path,
and no census, ratchet or corpus would have shown it. Same shape:
`java/nio/file/Paths.get(Ljava/lang/String;)` (the JDK's is varargs) and
`jdk/internal/net/http/Http1HeaderParser.parse([B)Z` (the JDK's takes a
`ByteBuffer`).

## The one deletion that was right, and the bug underneath it

`sun/nio/ch/FileChannelImpl.open(…ZZZLjava/lang/Object;)` was **not** live. No
JDK has ever declared an `Object`-tailed `open`, so the registration could never
bind — and `wp3_3_register_file_channel_real_smoke` asserted that exact spelling
under the message *"JDK 21 FileChannelImpl.open bridge must be registered"*. The
pin and the registration agreed with each other and with nothing else.

The real signatures, from `javap -p -s`:

| image | descriptor |
|---|---|
| Temurin 21.0.12+8 | `(Ljava/io/FileDescriptor;Ljava/lang/String;ZZZLjava/io/Closeable;)Ljava/nio/channels/FileChannel;` |
| Temurin 25.0.4+7 | `(Ljava/io/FileDescriptor;Ljava/lang/String;ZZZZLjava/io/Closeable;)Ljava/nio/channels/FileChannel;` |

Only the JDK 25 shape was registered. **On a JDK 21 image
`FileChannelImpl.open` had no usable registration at all**, and had not since
the `Object` spelling was written. The fix is the real descriptor, not the
deleted line; the test now asserts it, plus a negative assertion that the
unbindable spelling does not come back — otherwise the next person to "restore"
it turns the test green for the wrong reason again.

That is the useful part of the whole episode: **the deletion was right, and
deleting it is what exposed the gap.** A silent hole became a red test.

## What shipped

* **65 registrations restored** — 49 `Bridge`, 16 `SyntheticStub` (their
  receivers are on no image, so `no_image_receiver` re-tags them on the way in).
  Restored by reverse-applying only the pure-deletion hunks, so `dc55e8057`'s
  `ensure_synthetic_class` removal is untouched.
* **`FileChannelImpl.open` corrected** to JDK 21's real descriptor, with the
  no-Object-tail negative assertion.
* **`scripts/jdk-only-dead-sweep.py --pinned` takes a directory.** It read one
  file, `registry_contracts.rs`, which is why the pins in eight other files were
  invisible. It now walks the tree and understands three literal shapes:
  three-element tuple tables, direct `.find("class", "m", "(d)")` assertions,
  and `(method, descriptor)` pair tables looped over a class named in the
  `.find`. Trailing commas matter — rustfmt breaks a 3-tuple across four lines
  and the first version of this parser missed every one of them.

  **Measured, both directions**: pins 38 → **2,488**; deletion candidates
  326 → **292**; and all eight triples this record restores are off the list
  under the wide source, where three of them were still on it under the narrow
  one. That intermediate result is why this is a measurement and not a claim —
  the first fix looked complete and protected five of eight.

## Measured

* `native-builtins --lib` **3,402 passed / 0 failed** (was 3,380/22);
  `native-io --lib` **438 / 0** (was 436/2).
* `--jdk-only` corpus **53 passed / 6 failed** (was 52/6) and compatible
  **36 / 0** (was 35/0) — two vectors *gained*, and the six failures are the
  pre-existing `dev` set: `RReflect`, `RChmKeySetView`, `RJdkHandles`,
  `RJdkReflect`, `RJdkForkJoin`, `RJdkJmx`.
* `bridge_without_acc_native` 8,977 → **9,026** and
  `BASELINE_SYNTHETIC_STUBS` 923 → **939**. Both ratchets rose and both were
  re-frozen with the reasoning in place; the rise is 65 registrations coming
  back, and it partially un-does an "improvement" that was partly illusory.
* `native-awt` 259 / 1 — `image::tests::get_rgb_oob`, pre-existing since the L5b
  record.

## What this says about the deletion list

Two of its three buckets have now been found unsafe as committed, for the same
underlying reason and a week apart:

| bucket | verdict |
|---|---|
| `class-absent` | not adjudicable — the VM mints the receiver on demand (2026-08-10) |
| `method-nowhere` | not adjudicable **on its own** — the method may be CratonVM's own addition to a real class (this record) |

What survives is `method-nowhere` **minus** everything a test pins, and the pin
source has to be the whole tree. The list is 292 rows now, down from 791, and
every reduction so far has come from finding another thing the census cannot
see rather than from deleting anything.
