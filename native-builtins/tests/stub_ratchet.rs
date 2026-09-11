// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! STUB-RATCHET GATE — a one-way compatibility ratchet on synthetic-stub natives.
//!
//! Project rule (see `docs/synthetic-vs-real-explained.md` and
//! `feedback_no_synthetic_stubs`): **no NEW synthetic stubs**. The native
//! overlay tags every registration with a [`NativeKind`]:
//!
//!   * `Intrinsic` — a correct fast-path for a hot method (kept forever).
//!   * `Bridge`    — a native the VM genuinely needs (OS syscalls, `sun.*`
//!                   internals, classes with no real bytecode). It *is* the
//!                   real behavior.
//!   * `SyntheticStub` — a fake: placeholder / approximate / wrong return
//!                   values, fabricated objects, or "fake main" launcher
//!                   short-circuits. These shadow correct real bytecode and
//!                   are the removal target.
//!
//! This test builds the **default native registry the way the VM does** —
//! every registrar `vm/src/vm/vm_init.rs`'s real-JDK arm calls, in its order,
//! behind its `set_drop_real_layout_synthetic` flag (see
//! [`register_boot_path`]) — censuses how many registrations are tagged
//! `SyntheticStub`, and asserts the count has not RISEN above a frozen
//! [`BASELINE_SYNTHETIC_STUBS`] constant.
//!
//! **That claim has been false twice, so it is now an assertion rather than a
//! claim.** [`the_censused_scope_is_vm_inits_boot_path`] reads `vm_init.rs` as
//! text and fails if the arm calls a registrar this file does not replay, or
//! runs two of them in the opposite relative order. A count baseline cannot
//! notice a registration outside its own scope — that is not a weakness of the
//! number, it is what "outside the scope" means — so the scope is the part that
//! has to be checked mechanically.
//!
//! Until 2026-08-05 it ran `register_essential_natives` and nothing else, under
//! that same claim, and so measured about four fifths of the registry: 165
//! where the boot registry held 549. The fix that day named six registrars and
//! stopped, and `vm_init` calls 48; on 2026-08-11 the remaining forty were
//! found the same way — a lane's prediction moved the number by zero.
//!
//! It is a *ratchet*: a change that ADDS a synthetic stub pushes the count over
//! the baseline and fails CI; a change that REMOVES one is welcome and only
//! requires lowering the baseline (see `stub-ratchet.md`).
//!
//! ## 2026-08-19: the count rose 31 and NOT ONE new fake was written
//!
//! `dev` was red at 1308 against 1277 (and 1318 against 1287 in the management
//! configuration — the same +31, measured, not extrapolated). The gate's own
//! failure text sends the reader to "make the new native a real
//! Bridge/Intrinsic". For this population that instruction is **exactly
//! backwards**, and the reason is worth more than the re-freeze.
//!
//! Diffing the stub LIST at the freeze commit against `HEAD`
//! ([`dump_synthetic_stubs`], which exists because of this) gives 33 triples
//! that are stub rows now and were not then, and 4 that stopped being stub
//! rows. Of the 33, **30 already existed as registrations and only changed
//! KIND, `Bridge` -> `SyntheticStub`.** Nothing was added. Every one of the 30
//! is a deliberate, documented re-label whose PURPOSE is that `--jdk-only`
//! drops the row so the JDK's own bytecode runs:
//!
//! * **14** — `java/util/ArrayList` (twelve), `java/util/Arrays$ArrayList.
//!   iterator` and `java/util/Collections.synchronizedMap`. These are entries in
//!   `native-api/src/retired_shadow.rs`'s table: shadows RETIRED after being
//!   adjudicated one by one, each measured live as an actually-taken
//!   `native-shadows-bytecode` row.
//! * **6** — `java/lang/Runtime.exec`, tagged at the site with
//!   `register_with_kind(.., SyntheticStub)` and the reason beside it: `exec`
//!   is ordinary bytecode on the image (`acc_native: false, has_code: true`),
//!   so by contract §1.4 the real method outranks any bridge.
//! * **7** — `java/util/function/{Predicate,Consumer,BinaryOperator}`'s default
//!   and static methods, put in an explicit `SyntheticStub` scope in
//!   `phases_late/streams.rs` so strict drops all of them and `java.base`'s own
//!   default methods run.
//! * **3** — `jdk/internal/{access,misc}/SharedSecrets` factories, re-tagged
//!   because `jdk/internal/misc/SharedSecrets` is in `NO_IMAGE_JDK_RECEIVERS`.
//!
//! The other 3 of the 33 are genuinely new triples, and two of them are a
//! RENAME rather than an addition: `SharedSecrets.getJavaUtilJarAccess` (both
//! spellings) leaves the list and `javaUtilJarAccess` joins it, which is the
//! JDK's actual accessor name. The third is
//! `javax/net/ssl/SSLSocketInputStream.skip(J)J`, on a carrier class with no
//! image counterpart — the same rule as the SharedSecrets alias.
//!
//! **So the gate counted a campaign's success as a regression.** A `Bridge`
//! that was always a fake is a LIE the audit cannot see; re-tagging it
//! `SyntheticStub` makes it visible, gateable, and dropped in strict mode. The
//! count going up is what that improvement looks like from here.
//!
//! ### What this gate cannot distinguish, stated so the next reader does not
//! ### repeat the wrong work
//!
//! A number cannot separate "someone wrote a new fake" from "someone correctly
//! labelled an old one", and those two want opposite responses. Until the gate
//! freezes a SET rather than a count, the reader has to make that distinction
//! by hand — which is a two-command job now and was an afternoon of `git blame`
//! before:
//!
//! ```text
//! git worktree add /tmp/freeze <the commit that last set the baseline>
//! cargo test -p cratonvm-native-builtins --test stub_ratchet dump_synthetic_stubs -- --nocapture
//! ```
//!
//! run in both trees, and `comm -23` the sorted `@@STUB` lines. Then, for each
//! added triple, check `native-api/src/retired_shadow.rs` and the registration
//! site's own comment BEFORE concluding a fake was added.
//!
//! Wire into CI with:
//!
//! ```text
//! cargo test -p cratonvm-native-builtins --test stub_ratchet
//! ```

use cratonvm_native_api::{NativeKind, NativeMethodRegistry};
use cratonvm_native_builtins::register_essential_natives;
use cratonvm_types::compat::CompatibilityMode;

/// Frozen upper bound on the number of `SyntheticStub`-tagged registrations in
/// the default (real-JDK) **boot** registry — every registrar `vm_init`'s
/// real-JDK arm calls, not just `register_essential_natives` and not just the
/// six this file replayed until 2026-08-11. See [`register_boot_path`].
///
/// This is the exact current observed count. The ratchet has zero slack: adding
/// one synthetic stub fails, while removing one requires lowering the baseline
/// in the same change to lock in the improvement.
///
/// ## How to (re)compute the baseline
///
/// The exact count is produced at runtime by this very test. Run it once **in
/// each configuration** and paste the const line it prints — it names the
/// constant, so a `management` number cannot land in the no-management slot:
///
/// ```text
/// cargo test -p cratonvm-native-builtins --test stub_ratchet -- --nocapture
/// cargo test -p cratonvm-native-builtins --features management \
///     --test stub_ratchet -- --nocapture
/// ```
///
/// The test prints
///
/// ```text
/// stub-ratchet [<config>]: <N> SyntheticStub registrations out of <T> total ...
/// stub-ratchet: const <BASELINE_CONST>: usize = <N>;
/// ```
///
/// Paste that second line over the matching constant and keep [`SLACK`] at
/// zero. **Do not hand-derive this number** — the two scope defects this file
/// has carried were both hand-derived registrar lists, and the 1038 it froze on
/// 2026-08-11 was six above what any run of it produced. See
/// `docs/contributing/stub-ratchet.md`.
///
/// # 157 → 165, 2026-08-05 (JDK-only wave 2, lane L7 item 4)
///
/// The ratchet moved **up**, and the explanation its own failure message asks
/// for is that this change added no fake: it re-labelled eight that were
/// already there and were hidden from this count by the wrong tag.
///
/// The eight are `java/util/function/Function.{identity,compose,andThen}`,
/// `UnaryOperator.identity`, and `Function$Identity.{apply,andThen,compose}`.
/// Every one of them fabricates a `Function$Identity` / `Function$AndThen` /
/// `Function$Compose` stand-in, and **no JDK declares any of those names** —
/// real `Function.identity()` is one line of invokedynamic returning `t -> t`,
/// and `compose`/`andThen` are default methods that return a lambda. They were
/// tagged `Bridge`, which asserts "no working real-bytecode fallback exists".
/// There is one, in `java.base`, and `--jdk-only` now runs it: the
/// `JdkOnlyBreadthProbe` `lambdas` line is byte-identical to HotSpot 25.
///
/// So the count rising is this gate becoming *more* honest, not less: the
/// backlog it exists to measure was under-reported by eight. The direction to
/// be suspicious of is a `SyntheticStub` quietly becoming a `Bridge`, which
/// lowers this number while changing nothing — and which is exactly the shape
/// L6's unadjudicated-`Bridge` ratchet is being built to catch.
///
/// # 165 → 549, 2026-08-05 — a scope fix, not 384 new stubs
///
/// **Nothing was added.** The census stopped measuring `register_essential_natives`
/// alone and started running the boot sequence the VM actually installs, which
/// is 2,279 registrations wider. The 384 extra `SyntheticStub` rows were always
/// in the shipped registry; this gate simply could not see them.
///
/// The bug was found by disagreement, which is the useful part: L7's retag moved
/// 364 registrations `Bridge` → `SyntheticStub`, L6's `bridge-ratchet.sh`
/// counted every one (it censuses a *running VM*), and this gate did not move by
/// a single row. Two ratchets over one VM, 364 apart. Whenever two gates over
/// the same object disagree, at least one is measuring the wrong object.
///
/// **This is the number to compare against `bridge-ratchet.sh` from now on.**
/// If they diverge again, suspect the scope of one of them before suspecting
/// the code.
///
/// # 549 -> 554, 2026-08-05 (catching up with `14145d874`)
///
/// Five, and the change that made them is explicit about why:
/// `fix(jdk-only): the five duplicate registrations that outlived L7 R1's
/// retag` moved `ArrayList.subList`, `Collections.unmodifiableList` and
/// `{List,Set,Map}.copyOf` from the ambient `Bridge` to `SyntheticStub`, so
/// `--jdk-only` drops them and `java.base`'s own bytecode runs. It re-froze
/// L6's Bridge ratchet for the same five (9,705 -> 9,697) and did not re-freeze
/// this one, so `dev` has been failing this gate since — verified by running it
/// on a pristine `5efaa521c`, not inferred.
///
/// The direction is the same as the 157 -> 165 move above and wants the same
/// reading: **no new fake was added.** Five registrations that were mis-tagged
/// `Bridge` are now counted where they always belonged, and the real
/// improvement is on the other gate, which fell by eight.
///
/// # 554 -> 553, 2026-08-05 (a stub deleted, not re-tagged)
///
/// `java.util.Map.entry(K, V)` is no longer registered. A differential run
/// against HotSpot 25 (`probes/ShadowDifferentialProbe.java`) showed the stub
/// answering `java.util.Map$Entry@6c` where the JDK answers `k=7`, and
/// accepting a `setValue` the JDK's immutable `KeyValueHolder` refuses. The
/// method is ordinary bytecode in `java.base`, so deleting the registration is
/// the whole fix. This is the direction the gate exists to encourage: one
/// fewer fake, and the count follows.
///
/// # 553 -> 632, 2026-08-06 (lies removed from the census, not fakes added)
///
/// **No new synthetic stub exists, and no slot the VM dispatches changed kind.**
/// Measured, not asserted: two `--dump-native-registry` censuses from a real
/// JDK 25 boot, before and after, agree on the effective kind of all **10,639**
/// registered triples — zero differences (`scripts/jdk-only-kind-map.py`, and
/// a last-write-wins fold over both files).
///
/// This gate counts *registrations*, superseded rows included, and that is
/// where the 79 moved. Nine registrars had no `set_category` scope over them at
/// all, so each of their callers imposed its own — and registration is
/// last-write-wins, so what shipped was decided by call ORDER while the
/// superseded rows were left claiming a kind nothing ever used.
///
///  * `register_stamped_lock_natives` is called three times. Two callers had
///    `Bridge` in effect and one had none: 62 rows here (`StampedLock` and its
///    two view classes).
///  * `register_service_loader_natives`, `register_url_codec`,
///    `register_p59_bulk_stream_transfer` and the netty/tomcat/slf4j shim
///    registrars: the remaining 17.
///
/// All nine state `SyntheticStub` now, which is what their surviving row always
/// was and what the classes deserve — `java.util.ServiceLoader`,
/// `java.net.URLEncoder`/`URLDecoder`, `java.io.InputStream.transferTo` and
/// `java.util.concurrent.locks.StampedLock` are ordinary bytecode in
/// `java.base` and JDK 25 declares no `ACC_NATIVE` method on any of them, so
/// contract §1.5 cannot call them bridges.
///
/// **`--jdk-only` changed, and this is the fix, not the fallout.** A strict
/// census A/B shows 48 triples that strict mode used to ADMIT and now refuses,
/// and none in the other direction. It admitted them because the drop happens
/// at registration: the later `SyntheticStub` row was refused at the door and
/// the earlier `Bridge` row therefore survived to own the slot. Contract §11's
/// "zero synthetic-stub invocations through any path" was false for 48 triples,
/// by registration order, invisibly. See
/// the retired `native-kind-is-ambient-and-defaults-to-syntheticstub` write-up.
/// # 632 -> 642, 2026-08-06 (ten fakes `--jdk-only` was keeping, by file order)
///
/// The same shape as the re-freeze above, found by asking the question the
/// other way round: which triples does `--real-jdk` call a stub while
/// `--jdk-only` registers something? Twenty did, because a **second file**
/// registers the same triple under a `Bridge` scope, and strict mode refuses
/// the stub at the door — so the copy that survived to own the slot was the
/// one that was not a stub. Ten are now stated `SyntheticStub` at their second
/// site:
///
///  * `Function$Identity.apply` and `UnaryOperator.identity` in
///    `phases_late/streams.rs`. L7 item 4 retagged the `lib.rs` cluster; these
///    two copies kept `--jdk-only` minting a `Function$Identity` /
///    `UnaryOperator$Identity` whose class contract §5 forbids fabricating.
///    That is the "successor defect" the ambient-category record names, still
///    live in a file that lane did not open.
///  * five `CountDownLatch` and two `CyclicBarrier` registrations in a
///    surefire bootstrap block — the same callbacks `util_concurrent_ext`
///    installs and states as stubs.
///  * `java.io.InputStream.transferTo` in `native-io`, whose other copy in
///    `phases_late/zip_streams.rs` is a stub.
///
/// A strict census A/B: 10 triples refused that were admitted, none the other
/// way, and **zero** effective-kind changes in `--real-jdk`. The ten left
/// alone are `java.util.Set.of`, whose strict kind is `Intrinsic` — an
/// intrinsic shadowing concrete bytecode is what an intrinsic is (554 of 678
/// `Intrinsic` rows do), so that is a reclassification question, not a fake.
///
/// # 642 -> 644, 2026-08-06 (JDK-only wave 2, the ThreadPoolExecutor retirement)
///
/// Two, and they are the same shape as the 157 -> 165 move: **no new fake was
/// added.** `native_es_execute` — registered on both
/// `java/util/concurrent/ExecutorService.execute(Runnable)` and
/// `java/util/concurrent/ThreadPoolExecutor.execute(Runnable)` — was inheriting
/// the ambient `Bridge`, and it is not a bridge to anything: it is a
/// compatibility stand-in for CratonVM's synthetic 2-field
/// `Executors.new*ThreadPool()` receiver shape, which real `execute()` bytecode
/// would NPE on.
///
/// The retag is the load-bearing half of a change that DELETED code: eight
/// hand-written "does this receiver's `workers` field hold an object?" probes
/// across four files in `vm`, plus the ninth, receiver-blind
/// `force_native_over_real_jdk_bytecode` arm they existed to override. With the
/// native tagged `SyntheticStub` and `ThreadPoolExecutor` on
/// `real_protected_stub_class_common`'s allow-list, the one centralised
/// arbitration yields it to the real `execute()` body class-scoped, for every
/// receiver — and under `--jdk-only` both registrations are refused outright,
/// which the strict refusal count reflects (up 2).
///
/// This is what "wave 1 is measurement" buys: the count rises because the
/// census finally sees a fake it had mislabelled, at the moment that fake stops
/// being reachable on a real-JDK image.
///
/// # 644 -> 689, 2026-08-06 (giving `--jdk-only` a real `java.lang.ProcessImpl`)
///
/// Forty-five, and again **no new fake was added** — 45 registrations that
/// were already fakes stopped being labelled `Bridge`. They fall in three
/// groups, and the count is worth reading as three numbers rather than one:
///
///  * **29** on `cratonvm/synthetic/{Process, ProcessPipeInputStream,
///    ProcessPipeOutputStream, ProcessExitWaiter}` and
///    `cratonvm/synthetic/AnonymousObject$2` — receivers this VM mints and no
///    image contains, so §1.5's "what an `ACC_NATIVE` method binds to" has
///    nothing to point at. `Bridge` was keeping a fabricated class reachable
///    under `--jdk-only`, which is what §5 forbids outright. (The adjudication
///    that opened this counted 37; `58f0ffb30` deleted eight of them —
///    `phases_late`'s duplicate `cratonvm/synthetic/Process` surface — while
///    this change was in flight, so the retag lands on the 29 that remain.)
///  * **11** on `java/lang/ProcessBuilder` — every one shadowing ordinary
///    bytecode (`acc_native: false, has_code: true` for all eleven), so §1.4
///    gives the real method precedence. The cluster has to move together:
///    restating `start()` alone leaves `<init>([Ljava/lang/String;)V` writing a
///    raw `String[]` into the `command` field, and the JDK's own `start()` then
///    dies on `command.toArray(...)`.
///  * **5** `java.io` constructors — `BufferedOutputStream(OutputStream)` and
///    its sized twin, `FilterOutputStream(OutputStream)`, and
///    `FileInputStream`/`FileOutputStream(FileDescriptor)`. Each shim assigns
///    the wrapped stream and stops, while each real constructor also
///    initializes `private final Object closeLock = new Object()` — and every
///    matching `close()` opens with `synchronized (closeLock)`. A stream built
///    through these shims throws NullPointerException, not IOException, on its
///    first close, which `ProcessImpl.destroy`'s `catch (IOException ignored)`
///    cannot absorb.
///
/// What the 53 bought, measured rather than argued: `--jdk-only` now returns a
/// real `java.lang.ProcessImpl` from `ProcessBuilder.start()`, and
/// `probes/RealProcessSurfaceProbe` — 21 lines covering pid, all three streams
/// in both directions, `redirectErrorStream`, file and INHERIT redirects,
/// `onExit`, `destroy`, timed `waitFor`, an empty argument and an unspawnable
/// command — is **byte-identical to HotSpot 25**. Compatible `--real-jdk` mode
/// is byte-identical to the build before the change; it keeps every one of
/// these registrations and still answers with the VM's own process object.
///
/// # 689 -> 923, 2026-08-10 (the receivers no supported image declares)
///
/// Two hundred and thirty-four, and for the fourth time in this history **no new
/// fake was added.** This is the 2026-08-06 `cratonvm/synthetic/Process*` retag
/// above applied to the rest of its own family instead of to one cluster: a
/// registration whose receiver class is declared by **no** supported JDK image
/// cannot bind to an `ACC_NATIVE` method, so §1.5 does not admit it as a
/// `Bridge` under any reading. `native-api/src/no_image_receiver.rs` carries the
/// table, the six images (21 and 25 × linux, windows, macos) it was measured
/// against, and the two exclusions.
///
/// The count differs from the 248 a real-JDK boot census reports, and the
/// difference is this file's registry rather than the change: the ratchet runs
/// the default registration passes with no image, so it registers some triples a
/// real boot does not and misses others. Both numbers cover the same receiver
/// classes.
///
/// Measured, in both directions:
///
///  * `--jdk-only` regression corpus **52 passed / 6 failed** and compatible
///    mode **35 passed / 0 failed** — both **identical** to a binary built from
///    `dev` without the change, run on the same host against the same images.
///  * The `CRATONVM_NO_STUBS` drop list grows by exactly the 248 retagged rows
///    with **nothing else moving in either direction** — the check the
///    2026-07-14 `java.util.Properties` regression would have failed.
///  * One exclusion survives: the proxy machinery, as a reviewed VM service.
///    Four more were held back at first because strict mode still *created*
///    those classes, and were released when `ensure_synthetic_class` was deleted
///    the same day. The corpus found two of the five; the other three were
///    latent and came from a class-origin census, because no vector builds an
///    atomic field updater.
/// # 923 -> 939, 2026-08-11 (putting back what a census could not adjudicate)
///
/// Sixteen, and this one moves the ratchet in the direction it exists to
/// question, so the reasoning matters more than usual: **these sixteen are
/// registrations that already existed and were deleted three weeks' worth of
/// commits ago by mistake.** `dc55e8057` removed 179 rows its dead-sweep scored
/// `method-nowhere` — the class is on the image, the method is not — and 24
/// tests across two crates went red naming the triples they pin.
///
/// The sixteen that land here are the subset whose receiver class is on no
/// supported image at all (`java/lang/Compiler`, `java/rmi/activation/*`,
/// `sun/reflect/Reflection`, `java/net/InetAddressImplFactory`,
/// `java/security/AccessController$1`), so `no_image_receiver` tags them
/// `SyntheticStub` on the way back in. The other 49 restored rows are `Bridge`
/// and show up on L6's ratchet instead.
///
/// (This paragraph is PROSE, not an assertion — F17-1 read it as a pin blocking
/// the `AccessController$1` deletion, and F24-1 corrected that. It is history
/// and stays as written. For the record: two of the sixteen,
/// `AccessController$1.doIntersectionPrivilege` and `.getProtectDomains`, were
/// deleted outright on 2026-08-13 — JEP 486 removed the interface, the
/// implementation class and the accessor, so the restore was correct at the time
/// and the rows had nothing left to stand in front of. See cause (d) on
/// [`BASELINE_SYNTHETIC_STUBS_MANAGEMENT`].)
///
/// What the census could not see, in three shapes:
///
///  * **A stand-in for a bytecode method is `method-nowhere` by construction.**
///    Every `SharedSecrets` owner in `shared_secrets_bridge.rs` is one of the
///    JDK's own anonymous `Java*Access` classes whose methods are ordinary
///    bytecode; CratonVM registers stand-ins. Thirteen were deleted.
///  * **A `<clinit>` no-op shim can never be declared native.**
///    `Provider$ServiceKey.<clinit>` scores `method-nowhere` on every image
///    there will ever be.
///  * **A deliberate convenience overload is invisible.**
///    `Preconditions.checkIndex(II)I` exists so `String.charAt` does not pay a
///    Java frame per character; the JDK only declares the
///    `BiFunction`-taking form. Its own test says so in a comment.
///
/// Measured: `native-builtins --lib` 3,380/22 -> **3,402/0** and `native-io
/// --lib` 436/2 -> **438/0**; the `--jdk-only` corpus goes 52 passed to **53**
/// and compatible 35 to **36**, with the same six pre-existing failures. So the
/// rise buys back two crates of unit tests and two corpus vectors.
///
/// # 939 -> 1038, 2026-08-11: the first §1.4 shadow RETIREMENT
///
/// This ratchet's own message says "make the new native a real Bridge/Intrinsic
/// instead of a fake — do NOT just raise the baseline", and that is the right
/// instruction for the case it was built for: a NEW stub arriving. This rise is
/// the opposite motion and the ratchet cannot tell them apart, because it
/// counts stubs and both directions move the count.
///
/// 99 registrations went `Bridge` -> `SyntheticStub` and not one of them is
/// new. They are `java/util/logging/` triples whose target the JDK image
/// declares WITH A `Code` ATTRIBUTE — natives standing in front of real
/// bytecode, which is what contract §1.4 calls a shadow and what
/// `NativeKind::SyntheticStub` means. `--jdk-only` now refuses them and the
/// real class runs; `--real-jdk` is unchanged.
///
/// The re-tag is a MEASUREMENT, not a judgement, and the acceptance evidence is
/// the strict corpus rather than this count: armed through
/// `CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/logging/` the corpus is 23 passed
/// / 4 failed with the failing set unchanged, and with the retirement live it
/// is the same 23/4 while the core corpus is 37/0. L6's bridge ratchet moved
/// the other way by exactly the same population: `bridge_without_acc_native`
/// 9,015 -> 8,911 and `bridge_shadows_bytecode` 6,170 -> 6,066.
///
/// The table is `native-api/src/retired_shadow.rs`, which states each triple
/// and holds one back by name. Record: the retired
/// `bridge-reclassification-wave` write-up, item 2.
///
/// # 1038 -> 1263 / 1253, 2026-08-11: the gate could not see 40 of `vm_init`'s
/// 48 registrars
///
/// **A scope fix, for the second time, and nothing was added.**
/// [`register_boot_path`] carries the full account. The short form: the
/// 2026-08-05 widening named six registrar calls, `vm_init`'s real-JDK arm
/// makes 48, and so forty registrars' worth of registrations sat outside an
/// assertion documented as exact with zero slack — free to be added, deleted or
/// retagged `Bridge` <-> `SyntheticStub` with no effect on the number. That is
/// the whole failure a ratchet exists to prevent, and it surfaced through the
/// same instrument as last time: disagreement. A lane retagging the
/// `ProcessHandle` block predicted 1038 -> 1041, and after its work merged the
/// gate reported 7 passed / 0 failed with an unmoved count.
///
/// The measured move is 1032 (the live count under the old scope, itself six
/// BELOW the frozen 1038 — see [`MEASURED_CONFIG`]) to **1253**, and the 221
/// split cleanly by cause:
///
///  * **21 rows are the `ProcessHandle` retag** (`0ab1067ec`) — the movement
///    that change was entitled to and could not produce: 18 in
///    `register_p60_process_handle` and the three `current`/`pid`/`isAlive`
///    restatements in `register_phase57_process`, every one of them `Bridge`
///    before it. The old census held **zero** `java/lang/ProcessHandle` rows of
///    any kind, so the retag was not mismeasured, it was unmeasured.
///  * **200 rows predate that retag entirely** and were never counted by
///    anything here. They are not one registrar's backlog: the largest
///    contributors are `messaging_shims.rs`, `logging_shims.rs`,
///    `logmanager.rs`, `plain_socket.rs`, `atomic_updater.rs`,
///    `spring_startup_bootstrap.rs` and `native-io/src/process.rs`.
///
/// Measured directly rather than inferred: of the 221, **204 sit on triples the
/// old census did not hold at all** and the other 17 are second registrations
/// of triples it did. **Zero** sit on a triple the old census counted as a
/// NON-stub. So no row changed meaning and no kind was re-decided by this
/// change — the population grew, which is the only reading a scope fix admits.
///
/// The number is now FEATURE-dependent, and that is a property of `vm_init`
/// rather than a wart of the replay: its real-JDK arm gates ten `jmx::*`
/// registrars on `#[cfg(feature = "management")]`, `cratonvm-vm` enables that
/// feature by default so every shipping `cratonvm-cli` build has it, and a
/// `-p cratonvm-native-builtins` resolve does not. Collapsing the two into one
/// number would mean dropping those ten registrars from the model in BOTH
/// configurations, which re-opens the blind spot deliberately. So the baseline
/// is keyed per configuration, as `duplicate_registration_gate.rs` keys its
/// two.
///
/// # PENDING, 2026-08-12: this baseline is STALE and expected to FIRE by +6
///
/// **Not re-frozen here, on purpose.** The session that found it could not run
/// `cargo`, and raising a slack-free exact baseline from a derivation rather
/// than from the printed recount line is the one edit that can do damage: too
/// high and it admits that many new stubs in silence. So the number stays and
/// the reason is written down. W7-62-ratchets-and-dead-code.md
///
/// The freeze at `167bf048c` (2026-08-11 22:05) IS an ancestor of `6ae3ca634`
/// (the `LinkedListSnapshotListItr` retag, which took nine rows OUT of this
/// population, so that one is already counted here) and is **not** an ancestor
/// of three commits that put rows in. Checked with
/// `git merge-base --is-ancestor`, not by timestamp:
///
///  * `4eaa5d321` (21:19) tags `LogManager.{getLogManager, getLogger}`
///    `Bridge`. Both triples are already in `RETIRED_SHADOW_TRIPLES`, so
///    `register()`'s retired-shadow arm lands them on `SyntheticStub`. **+2**
///  * `3b20b83b5` (22:34) tags four `LogRecord` source-pair rows and
///    `Formatter.formatMessage` `Bridge`. None was retired at that commit.
///    **+0**
///  * `01cfc2609` (2026-08-12 03:18) adds the four `LogRecord` source-pair
///    triples to `RETIRED_SHADOW_TRIPLES`, so their `Bridge` becomes
///    `SyntheticStub`. **+4**
///
/// All of them are inside this census's scope:
/// `register_phase54_logging_extras` is reached from
/// `register_essential_natives_with_shims`, the first entry in
/// [`VM_INIT_SEQUENCE`].
///
/// So expect **1269 / 1259**, and expect this gate to fail until re-frozen.
/// **That rise is the 939 -> 1038 motion again, not the motion this gate
/// guards against** — see the "first §1.4 shadow RETIREMENT" section above.
/// Six registrations moved `Intrinsic` -> `SyntheticStub` and not one of them
/// is new: each stands in front of `java/util/logging/` bytecode the image
/// declares with a `Code` attribute, and re-tagging is what lets `--jdk-only`
/// refuse the shadow and run the real class. `--real-jdk` is unchanged,
/// because a `SyntheticStub` registers and dispatches normally there.
///
/// **+6 is a LOWER BOUND, and must not be pasted in as the answer.** 182
/// commits separate the freeze from HEAD and any of them may add or remove a
/// registration. Take the number from the line this test prints:
/// `cargo test -p cratonvm-native-builtins --test stub_ratchet -- --nocapture`,
/// then paste the constant named by `BASELINE_CONST` in that same run — it
/// says which of the two this build adjudicates against.
///
/// ## The red has more than one cause — attribute them BEFORE re-freezing
///
/// Three separate wave-7 changes move (or deliberately do not move) this
/// number. A single conflated re-freeze is what made the sibling
/// `jdk-only-bridge-ratchet.json` unreadable, so they are listed apart:
///
///  * **(a) the +6 above** — `4eaa5d321` and `01cfc2609`, six `java/util/logging/`
///    registrations that became `SyntheticStub` because they were added to
///    `RETIRED_SHADOW_TRIPLES`. A retirement, not a new fake.
///  * **(b) the four new scalar `StringBuilder.insert` overloads** (`IZ`/`IJ`/
///    `IF`/`ID`, registered on `StringBuilder` / `StringBuffer` /
///    `AbstractStringBuilder`) move this number by **ZERO**. The ambient kind at
///    that registration site is `Bridge`, so the twelve rows land outside this
///    census's population entirely. They DO move
///    `scripts/baselines/jdk-only-bridge-ratchet.json` and the kind map. Stated
///    here because "a registrar grew, so every ratchet moved" is the wrong
///    default assumption and costs a re-freeze to unlearn.
///  * **(c) the 7-row `java/io/Print*` shadow retirement, NOT LANDED.** If it
///    lands, its rows join `RETIRED_SHADOW_TRIPLES` and this number rises by up
///    to seven, in the (a) direction. It was deliberately held back because it
///    moves three artefacts frozen at `25/linux` — this constant,
///    `bridge_shadows_bytecode` and the kind map — which must be re-frozen in
///    ONE commit from ONE Linux census. Do not re-freeze this constant "for"
///    the retirement before the retirement exists.
///  * **(d) F33-1, 2026-08-13: net +2, both halves attributed.** Two changes in
///    `native-builtins/src/shared_secrets_bridge.rs`, in opposite directions:
///
///    **+4.** The four `SharedSecrets` accessors whose owner is a
///    `cratonvm/internal/ss/…$1` stand-in (`javaUtilJarAccess`,
///    `getJavaNetUriAccess`, `getJavaNetHttpCookieAccess`,
///    `getJavaIORandomAccessFileAccess`) move `Bridge` → `SyntheticStub` on
///    `jdk/internal/access/SharedSecrets`, so `--jdk-only` refuses a factory
///    whose owner it already dropped. Their `jdk/internal/misc/SharedSecrets`
///    twins move by **zero** — that class is in `NO_IMAGE_JDK_RECEIVERS`, so
///    `register()` was already re-tagging them. This is the (a) direction: a
///    fake being *labelled*, not a fake being added.
///
///    **−2.** `register_java_security_access` is deleted, taking
///    `java/security/AccessController$1.doIntersectionPrivilege` and
///    `.getProtectDomains` out of the population. Those are two of the sixteen
///    the 923 → 939 note above restored, so that paragraph's arithmetic is now
///    fourteen; it is left as written because it is history, and rewriting a
///    recount narrative to match a later tree is how a ratchet's provenance
///    stops being checkable.
///
///    Derived, NOT measured — this lane may not run `cargo`. So **+2 is a
///    prediction to check against the printed line, not a number to paste**, and
///    it composes with the +6/+8 of (a) rather than replacing it. (The totals
///    this paragraph originally named were computed against the pre-re-freeze
///    constants and are superseded by the merge note at the bottom; the +2
///    itself is unchanged, because it is a delta and not a total.)
///    F33-1-a-factory-and-its-owner-must-share-one-kind-20260813.md
///
/// Anything the run reports beyond (a), (d) and the re-freeze below is a
/// finding to attribute, not slack to absorb: 182 commits separate the freeze
/// from HEAD.
///
/// ## Re-frozen 2026-08-13: 1263 -> 1287, and 1253 -> 1277
///
/// **+24 in BOTH configurations, from ONE change**, which is why both constants
/// move by the same delta in one commit: twelve `Collection` methods forwarded
/// on each of `java/util/Collections$SynchronizedCollection` and
/// `$SynchronizedSet` (`register_synchronized_collection_wrapper_natives`,
/// bodies in `util_concurrent_ext::sync_collection_delegate`).
///
/// **Attributed, not absorbed.** The wrapper had seven methods —
/// `add`/`contains`/`remove`/`size`/`isEmpty`/`iterator`/`toArray` — which was
/// the whole surface `Collections.synchronizedCollection(…)` had ever been asked
/// for. Since 2026-08-13 the wrapper is also what `Hashtable`/`Properties` hand
/// back for `keySet()`/`entrySet()`/`values()` (the JDK's own answer: those
/// accessors are `Collections.synchronizedSet(new KeySet(), this)`), so it is
/// now asked for `stream`, `forEach`, `toString`, `containsAll`, `retainAll`,
/// `removeAll`, `addAll`, `removeIf`, `clear`, `spliterator` and the two typed
/// `toArray` overloads as well.
///
/// **`SyntheticStub` is the right kind here, and `Bridge` would be wrong.** In
/// real-JDK mode all 24 are DROPPED — the strict census in this same file
/// reports every stub row dropped — and the real
/// `Collections$SynchronizedCollection` bytecode runs, `synchronized (mutex)`
/// included. They exist for synthetic-JDK mode, where the class has no bytecode
/// at all and an UNDECLARED method falls through to an interface-level native
/// that reads the WRAPPER as the collection and reports it EMPTY. Registering
/// them `Bridge` would make them win over the real class and silently drop the
/// synchronization. See
/// `collection-view-carrier-residuals-FIXED-20260813.md`.
///
/// ## Merge 2026-08-16: the constants below are the re-freeze, and (d) is NOT in them
///
/// The `--jdk-only` branch and mainline reached this constant from two
/// different trees and neither number is the merged tree's number:
///
///  * the re-freeze above (1263 → **1287**, 1253 → **1277**) was MEASURED, on a
///    tree that did not yet contain (d)'s `shared_secrets_bridge.rs` change;
///  * (d)'s **+2** was DERIVED, on a tree that did not yet contain the 24
///    `SynchronizedCollection`/`SynchronizedSet` rows.
///
/// The two touch disjoint populations — `jdk/internal/access/SharedSecrets`
/// factories and `java/security/AccessController$1` on one side, the
/// `Collections$Synchronized*` wrappers on the other — so the merged census
/// should be their SUM: no-management **1279**, management **1289**.
///
/// That sum is arithmetic, not a measurement, so it is deliberately NOT pasted.
/// The constants stay at the two numbers that were actually measured. Read a
/// first post-merge run this way: **exactly +2 over these constants is (d)
/// landing and is the expected result — re-freeze to 1289 / 1279 and delete
/// this section. Any other delta is a finding to attribute.**
/// # 1287 / 1277 -> 1396 / 1386, 2026-08-19 — 78 relabels and 31 inherited
///
/// **The gate had been RED since before this session and was therefore gating
/// nothing.** `G83-1` recorded it 31 over its baseline on 2026-08-19 and
/// stopped there, because naming the 31 needed a script nobody had written. A
/// failing assert cannot notice a 32nd stub: every synthetic stub added between
/// 2026-08-14 and today entered a tree whose ratchet was already failing. That
/// is the reason to re-freeze, and the enumeration below is the price of doing
/// it honestly — a re-freeze that does not say what it absorbs is how the 31
/// got in.
///
/// ## How this was measured
///
/// [`synthetic_by_file`] and the `CRATONVM_RATCHET_ROWS=1` row dump added to
/// [`synthetic_stub_count_does_not_regress`] were run at THREE commits in a
/// detached worktree — `8c6801820` (where 1277 was frozen, 2026-08-14),
/// `8a7e2727f` (the last commit before this session), and HEAD — and the row
/// sets diffed by `(file, class, method, descriptor)`:
///
/// ```text
///   8c6801820   1277 stubs / 12780 rows
///   8a7e2727f   1308 stubs / 12792 rows     +31 stubs, +12 rows
///   HEAD        1386 stubs / 12792 rows     +78 stubs,   0 rows
/// ```
///
/// **The row-count column is the whole argument.** A relabel keeps its row and
/// moves only its kind; a genuinely new fake adds a row. This gate freezes one
/// number and so cannot tell the two apart — and they have opposite signs. The
/// second column can, and it is why the two deltas below are read differently.
///
/// ## The 78 (this session): every one a relabel, total registrations unchanged
///
/// 78 rows added, **0 removed, and the registry is the same 12792 rows it was**.
/// Not one new native was registered; 78 already-registered fakes stopped
/// claiming to be `Bridge`. This is the direction the *157 -> 165* note above
/// records as the gate becoming more honest, at 10x the scale:
///
/// ```text
///   +28  native-collections/src/lib.rs      java/util/Vector, all 28   (5546e0b7c)
///   +27  native-io/src/watch.rs             the WatchService surface   (G88-1 §9)
///   +12  native-io/src/stream_encoder.rs    sun/nio/cs StreamEncoder
///   +10  native-io/src/stream_decoder.rs    sun/nio/cs StreamDecoder
///   +1   native-io/src/lib.rs               the string reader/writer shim
/// ```
///
/// The `watch.rs` 27 are the retag `G88-1` §9 declined for want of an exercise
/// and `G85-1` §3b's rule refused to accept unverified; `RJdkWatchService`
/// (13 checks, scheduled) is that exercise. Note that this census is a THIRD
/// instrument agreeing with `--dump-native-registry` and the arms on the same
/// 27 — the in-process boot replay, which neither of the other two is.
///
/// It also disagrees usefully with the runtime dump on scale: the dump reported
/// 460 `native-collections` stubs "was 0", this replay had 432 of them before
/// the session started. Both are right about their own population. Neither is
/// "the" number, which is the standing reason this file names its configuration
/// beside every count.
///
/// ## The 31 (inherited, 2026-08-14 .. 2026-08-18): named, not absorbed silently
///
/// 35 rows added, 4 removed, and the registry grew by 12 — so **at most 12 of
/// the 35 are new registrations and at least 23 are relabels**. They are not
/// this change's work and are re-frozen only because the alternative is a gate
/// that stays dead. By file:
///
/// ```text
///   +14  native-collections/src/lib.rs
///          java/util/ArrayList x12 (<init> x3, add, clear, contains, get,
///          isEmpty, iterator, size, toArray x2), Arrays$ArrayList.iterator,
///          Collections.synchronizedMap
///   +8   native-builtins/src/lib.rs
///          java/lang/Runtime.exec x6 (every overload),
///          java/util/ArrayList.{<init>(I)V, iterator}
///   +7   native-builtins/src/phases_late/streams.rs
///          Predicate.{and,or,negate,not}, Consumer.andThen,
///          BinaryOperator.{minBy,maxBy}
///   +1   native-builtins/src/shared_secrets_bridge.rs   (+5 rows, -4 rows)
///   +1   native-builtins/src/phases_late/ssl_security.rs
///          javax/net/ssl/SSLSocketInputStream.skip(J)J
/// ```
///
/// The seven in `phases_late/streams.rs` are the same shape as the eight that
/// moved this constant 157 -> 165: `java.util.function` DEFAULT methods, which
/// no JDK declares `native` and which real bytecode already implements as one
/// line returning a lambda. They are the strongest removal candidates in the 31
/// and the cheapest, being interface defaults with no state.
///
/// **The prediction in the merge note above is discharged, and it was off by
/// one.** That note said "exactly +2 over these constants is (d) landing …
/// any other delta is a finding to attribute". Measured: (d)'s file moved +5
/// rows and -4, i.e. **+1**, not +2 — and 30 further stubs arrived from other
/// merges in the same window. That is the attribution it asked for.
///
/// ## What must NOT be read into this re-freeze
///
/// It does not adjudicate the 31 as necessary. `G83-1` N1 stands — find them
/// and remove them — and it is now actionable rather than a search, because the
/// list is above. Nor does it license a future re-freeze: the next rise must
/// come with the same two columns (stubs AND rows) and the same enumeration,
/// which is why the failure message now prints the per-file breakdown itself.
/// # 1396 / 1386 -> 1622 / 1611, 2026-08-19 — the §1.4 shadow wave, and the
/// first time the two-column rule adjudicated a change instead of explaining one
///
/// 227 triples over five subsystems were RETIRED as contract §1.4 shadows
/// (`native-api/src/retired_shadow.rs`, `G90-1`): a native standing in front of
/// concrete JDK bytecode now yields to it under `--jdk-only`. Retirement is
/// implemented as a re-tag to `SyntheticStub` at registration, so this count
/// rises by exactly the number retired on the boot path — **+226 management,
/// +225 no-management, and the registry did not grow by ONE ROW** (13160 and
/// 12792, both unchanged).
///
/// That is the whole adjudication, and it took no recollection of what the
/// change did. The rule added above on the same day says a relabel keeps its
/// row while a new fake adds one; the row column did not move; therefore every
/// one of the 226 is a relabel. The *157 -> 165* note further up argues the
/// same conclusion in eight paragraphs of prose about eight registrations,
/// because at the time there was no second number to point at.
///
/// **Read the direction correctly.** This is the largest single rise this
/// constant has ever taken and it is the most correct the registry has been:
/// 227 fakes stopped claiming to be `Bridge` AND stopped shadowing real
/// bytecode under `--jdk-only`. The acceptance test is
/// `regression-suite/run.sh` with `CRATONVM_ARGS=--jdk-only` — 102 / 102, the
/// baseline — and it is the test that matters, because it is the one that
/// rejected 28 further triples the 36-vector screen had passed
/// (`java/lang/ref/`, `sun/nio/fs/`; see `G90-1` §5).
///
/// # H3-1 REBASELINE — SUPERSEDED BY THE `H0 RE-FREEZE` NOTE BELOW
///
/// **This block is H3-1's PREDICTION, kept for the reasoning in it. The
/// prediction was WRONG and the measured outcome is in the next section —
/// read that one for the current value.** Predicted: old 1622, delta −7,
/// new 1615. Measured: **+4, not −7**, and the constant below is 1626.
/// The prediction is left standing rather than deleted because the reason
/// it was wrong is the useful part: it assumed the only change was its own
/// seven deletions, and a gate that had not run since 2026-08-14 was
/// hiding three other movements.** The seven
/// `java.util.function` default/static-method stubs `G89-1` N1 nominated were
/// DELETED from `native-builtins/src/phases_late/streams.rs`
/// (`Predicate.{and,or,negate,not}`, `Consumer.andThen`,
/// `BinaryOperator.{maxBy,minBy}`), so both this count and
/// [`MEASURED_TOTAL_REGISTRATIONS_MANAGEMENT`] fall by the same 7 — a genuine
/// **removal**, not a relabel, which is exactly the case the two-column rule
/// exists to distinguish.
///
/// **The number above is deliberately left at 1622 and is NOT to be taken as
/// the post-change count.** H3-1 could not build or run; a slack-free ratchet
/// with a hand-written value that lands too HIGH silently re-admits that many
/// new stubs, and one written from arithmetic rather than a run is exactly the
/// species this file's own history records ("a constant derived by arithmetic
/// from the other configuration sat six above the truth for a week"). Leaving
/// it high is the safe direction: the assert is `<=`, so the run PASSES and
/// prints `IMPROVED`-shaped output plus the exact constant to paste.
///
/// The one command that recomputes it — run BOTH, paste BOTH:
///
/// ```text
/// cargo test -p cratonvm-native-builtins --features management \
///     --test stub_ratchet synthetic_stub_count_does_not_regress -- --nocapture
/// cargo test -p cratonvm-native-builtins \
///     --test stub_ratchet synthetic_stub_count_does_not_regress -- --nocapture
/// ```
///
/// Each prints `stub-ratchet: const <NAME>: usize = <n>;` — paste that line.
/// Anything other than −7 in either configuration is a finding to attribute
/// before re-freezing: use `CRATONVM_RATCHET_ROWS=1` on this commit and on
/// `26e4b5db4` and diff the sorted `stub-ratchet(row):` lines.
/// # H0 RE-FREEZE — 2026-08-20, MEASURED, and the delta was NOT the predicted one
///
/// H3-1 predicted -7 in both configurations. **Measured: +4 in both**, with the
/// row column up 65. The whole delta is named, by diffing the `@@STUB` lines at
/// the freeze commit `083998c7b` against this tree -- 11 added, 7 removed:
///
/// **+8, cause (b), the opposite of a regression.** The eight
/// `sun/nio/fs/WindowsFileAttributes` accessors (`creationTime`,
/// `lastAccessTime`, `lastModifiedTime`, `isDirectory`, `isOther`,
/// `isRegularFile`, `isSymbolicLink`, `size`) were RETIRED as shadows by H2-1,
/// so they stopped claiming to be `Bridge` and now read `SyntheticStub` --
/// which means `--jdk-only` drops them and real `WindowsFileAttributes`
/// bytecode runs, reading the Windows FILETIME fields H2-1 taught the VM to
/// populate. **This wave's headline win arrives at this gate as a RISE.** Do
/// not read it as one; verified live, not inert: all eight print
/// `[JDK-ONLY-REFUSED]` under `CRATONVM_DBG_DROPPED_STUBS=1` and `RFileTimes`
/// still passes 68 checks.
///
/// **+3, pre-existing, arrived by merge.**
/// `cratonvm/internal/ss/JavaUtilJarAccess$1.{entryFor,getTrustedAttributes,isInitializing}`.
/// Not new fakes: that receiver is a **CratonVM-internal** SharedSecrets
/// carrier, so there is no real JDK class being shadowed and `SyntheticStub` is
/// the honest kind. They became COUNTED rather than written, because
/// `b7e24364f` corrected the accessor spelling (`javaUtilJarAccess`, no `get`
/// prefix -- the real method never carried one) and a door measured
/// `method-nowhere`-dead went live. They entered this tree through the
/// `origin/dev` merge, not through wave H.
///
/// **-7, a genuine removal.** H3-1's seven `java.util.function` default/static
/// stubs, exactly as it claimed.
///
/// The other ~61 of the +65 rows are NON-stub registrations from work that
/// landed on `dev` concurrently. They are visible here only because this gate
/// could not run: it was red from 2026-08-14 (`G89-1` §4) and then did not
/// PARSE at all from merge `26e4b5db4` until H3-1 repaired it, so this is the
/// first adjudication since `083998c7b`.
///
/// **An instrument finding, recorded because it will bite the next re-freeze.**
/// The failure message tells you to "run `dump_synthetic_stubs` here and at the
/// commit that last set the baseline, and diff the sorted `@@STUB` lines". That
/// was **impossible**: `dump_synthetic_stubs` did not exist at `083998c7b`. The
/// diff above was only possible by back-porting the function into a scratch
/// worktree at that commit. The procedure works for baselines set from now on;
/// it did not work for this one, and a procedure that cannot run reads exactly
/// like a procedure nobody bothered to run.
/// # 1622 / 1611 -> 1625 / 1614, 2026-08-21 — three methods on a carrier
/// `--jdk-only` already drops whole, and a third case this gate cannot state
///
/// The rise is exactly three rows, and for the first time they were IDENTIFIED
/// before the constant moved rather than argued about afterwards
/// (`dump_synthetic_stubs` here, and the same test grafted onto `083998c7b`
/// where it did not yet exist — `census_rows` is byte-identical across the two,
/// so the graft measures that commit's registry and nothing else):
///
/// ```text
/// cratonvm/internal/ss/JavaUtilJarAccess$1.entryFor(Ljava/util/jar/JarFile;Ljava/lang/String;)Ljava/util/jar/JarEntry;
/// cratonvm/internal/ss/JavaUtilJarAccess$1.getTrustedAttributes(Ljava/util/jar/Manifest;Ljava/lang/String;)Ljava/util/jar/Attributes;
/// cratonvm/internal/ss/JavaUtilJarAccess$1.isInitializing()Z
/// ```
///
/// Three added, ZERO removed, from `392e6990a` (2026-08-19), which completed
/// `jdk.internal.access.JavaUtilJarAccess`: the carrier had two of its five
/// methods and the other three were abstract on a synthetic class, so a caller
/// reaching one got an `AbstractMethodError`.
///
/// **The two-column rule does not adjudicate this one, and that is the finding.**
/// Totals moved 12792 -> 12885 and 13160 -> 13253, i.e. +93 rows against +3
/// stubs — neither "total unchanged" (a relabel) nor "total up by about the stub
/// delta" (new fakes). It is 90 new `Bridge` rows and 3 new stubs, and the 3 are
/// genuinely new registrations, not re-tags.
///
/// So by the letter of the message below this is case (a), "a NEW fake was
/// written — do NOT just raise the baseline". Both of that case's remedies are
/// unavailable, and not by accident:
///
/// * "make the new native a real `Bridge`/`Intrinsic`" would be WRONG here.
///   `cratonvm/internal/ss/JavaUtilJarAccess$1` is on
///   `no_image_receiver::VM_MINTED_STAND_IN_RECEIVERS`, and `F33-1` decided
///   deliberately that a factory and the carrier it hands out share one kind, so
///   the whole thing — factory and methods together — is refused under
///   `--jdk-only`. Tagging these three `Bridge` would keep a fake carrier alive
///   in strict mode, which is the defect that page exists to record.
/// * "prefer fixing the underlying VM gap so real bytecode runs" has nothing to
///   run. The receiver's class exists only inside CratonVM and has no bytecode
///   at all; there is no JDK body behind `entryFor` to yield to.
///
/// The alternative to these three stubs is therefore not real bytecode — it is
/// the `AbstractMethodError` they were added to stop. `--jdk-only` behaviour is
/// unchanged either way, because the carrier is dropped whole in both.
///
/// **The third case, for the next reader:** a stub added to a receiver that
/// strict mode already refuses IN ITS ENTIRETY costs strict mode nothing. Ask
/// what mints the receiver (the rule at
/// `no_image_receiver::VM_MINTED_STAND_IN_RECEIVERS`) before applying the
/// dichotomy below; if the owner is a listed stand-in, the row count is the only
/// thing that moved and completing its interface is a fix, not a regression.
///
/// Found while merging dev into an unrelated TLS branch, because this file had
/// not COMPILED since `26e4b5db4` — see
/// `known-issues/stub-ratchet-was-a-compile-error-and-is-three-over-baseline-20260820.md`.
/// The count had been three over for two days with nothing able to say so.
///
/// # 1625 / 1614 -> 1609 / 1598, 2026-08-21 -- measured by ROW DIFF, not argued
///
/// The `claude/jdk-only-mode-handoff-09b48c` merge. **-16 stubs in BOTH
/// configurations, and -70 total** against a row dump of pristine `be6e52f82`
/// (`CRATONVM_RATCHET_ROWS=1` on each tree, sorted, `comm`). This is the third
/// case -- registrations DELETED -- and for once the account is a list rather
/// than a subtraction:
///
/// ```text
/// GONE (24)
///   12  java/util/Comparator.{naturalOrder,reverseOrder,reversed,comparing,
///         comparingInt,comparingLong,comparingDouble,thenComparing x2,
///         thenComparingInt,thenComparingLong,thenComparingDouble}
///                                             native-collections/src/lib.rs
///    9  java/util/function.{Predicate.and,or,negate,not; Consumer.andThen;
///         Function.andThen,compose; BinaryOperator.maxBy,minBy}
///                                    native-builtins/src/phases_late/streams.rs
///    3  java/lang/invoke/MethodHandleProxies.{asInterfaceInstance,
///         isWrapperInstance,wrapperInstanceTarget}
///                                        native-builtins/src/lang_invoke.rs
/// NEW (8)
///    8  sun/nio/fs/WindowsFileAttributes.{isDirectory,isRegularFile,isOther,
///         isSymbolicLink,size,creationTime,lastAccessTime,lastModifiedTime}
///                                  native-builtins/src/phases_late/nio_file.rs
/// ```
///
/// 24 gone, 8 new, net -16. The twelve are the `java/util/Comparator` family
/// guarded behind `registry.drops_real_layout_synthetic()` so a REAL image runs
/// the JDK's own `Comparators$NaturalOrderComparator` instead of a stub -- the
/// closure of the last standing `SUITE=all` failure.
///
/// **CORRECTION.** The merge commit that landed this said the delta was NOT the
/// Comparator guard, "which is inert in this registry -- 9 Comparator rows are
/// still present". That is WRONG, and the way it was wrong is worth keeping:
/// the check was `grep 'java/util/Comparator'` over the row dump, which matches
/// DESCRIPTORS as well as classes. Seven of those nine survivors are other
/// classes that merely take a `Comparator` argument
/// (`cratonvm/internal/ArrayListSubList.sort(Ljava/util/Comparator;)V`,
/// `java/util/stream/Collectors.{minBy,maxBy}`); only
/// `java/util/Comparator$Native.{compare,writeReplace}` is the class itself,
/// and those two are registered unconditionally by design. **Grep the row's
/// CLASS field, not the line.** The guard fired exactly as intended.
///
/// **The +8 are the half that deserves scrutiny**, and they are case (a) by the
/// dichotomy below: new `SyntheticStub` rows on `sun/nio/fs/WindowsFileAttributes`,
/// a REAL JDK class rather than a VM-minted stand-in, so the
/// `VM_MINTED_STAND_IN_RECEIVERS` exemption recorded above does NOT cover them.
/// They are recorded here as owed work, not absolved: under `--jdk-only` these
/// eight are dropped and the JDK's own bytecode must satisfy them.
///
/// **The totals were re-measured, not carried.** The previous
/// `MEASURED_TOTAL_REGISTRATIONS_*` were themselves stale: pristine `be6e52f82`
/// measures **12919** total in the no-management resolve against a frozen
/// 12885, so dev's own second column had drifted +34 with nothing able to say
/// so. Both columns below are from the runs quoted in this block.
///
/// Measured post-merge on the merged tree (WORKER-5 included, which moved the
/// totals a further -15 and the stub counts NOT AT ALL):
///
/// ```text
/// management     1609 SyntheticStub out of 13202 total
/// no-management  1598 SyntheticStub out of 12834 total
/// ```
///
/// # 1609 / 1598 -> 1627 / 1616, 2026-08-22 -- one ABSTRACT spelling became four
/// CONCRETE ones, and the count rises because there are four
///
/// WORKER-4's abstract-receiver work. Row diff, same method as the block above
/// (`CRATONVM_RATCHET_ROWS=1` on each tree, sorted, `comm`):
///
/// ```text
/// GONE  (9)   sun/nio/fs/UnixWatchService.{init0,close0,register0,cancel0,
///               reset0,poll0,take0,pollEventKinds0,pollEventNames0}
/// NEW  (27)   the SAME nine methods on sun/nio/fs/BsdWatchService,
///               sun/nio/fs/LinuxWatchService and sun/nio/fs/MacOSXWatchService
///             all in native-io/src/watch.rs
/// net +18, and the same +18 in BOTH configurations
/// ```
///
/// **This is case (b), and a sharper form of it than the dichotomy below
/// states.** `sun/nio/fs/UnixWatchService` is ABSTRACT: no receiver can legally
/// be one (JVMS 6.5), so a registration there was answering for a class that
/// cannot exist and the concrete per-platform impl never reached its own row.
/// The registrations did not multiply -- the SPELLING did, from one illegal
/// receiver to the three legal ones that inherit from it. `WindowsWatchService`
/// is covered too (`watch.rs:884`) and is not in the diff because it already
/// carried its own rows.
///
/// **The count going UP is therefore the fix landing, not a regression**, and
/// this is the second shape in this file that the two-column rule cannot
/// adjudicate on its own. Totals moved 12834 -> 13260 (+426), far more than the
/// +18, because the concrete classes take the whole mirrored family and not
/// just the stub rows.
///
/// **Do not "fix" this by re-registering on the abstract class to get the
/// number down.** That is the defect. A per-platform stub count of 3N where the
/// abstract count was N is the correct shape for anything the JDK spells per
/// platform.
///
/// Measured post-merge:
///
/// ```text
/// management     1627 SyntheticStub out of 13628 total
/// no-management  1616 SyntheticStub out of 13260 total
/// ```
///
/// # 1627 / 1616 -> 1573 / 1562, 2026-08-22 -- FIFTY-FOUR rows retired, and the
/// same rows this file re-froze UPWARD nine hours earlier
///
/// The third case, at the largest scale this gate has recorded: -54 in BOTH
/// configurations, every row from `native-io/src/watch.rs`, deleted rather than
/// relabelled.
///
/// **This closes an arc that starts in the block above.** That entry re-froze
/// 1609/1598 -> 1627/1616 for an abstract->concrete respelling: nine
/// `UnixWatchService` rows became twenty-seven on `Bsd`/`Linux`/`MacOSXWatchService`
/// because a registration on an abstract class can never be dispatched. The
/// respelling was correct AND those twenty-seven are inside the fifty-four now
/// gone -- making a row legal to dispatch is what made it measurable, and the
/// measurement said none of them could ever fire.
///
/// **The evidence is the part to copy.** `WORKER-4-2` N5 had measured
/// `invocations: 0` and REFUSED to delete, because a census of the DEFAULT build
/// says nothing about `--features synthetic-jdk`. That refusal was right. This
/// round made the missing build:
///
/// ```text
///                             watch.rs rows   INVOKED   probe
///   --jdk-only                       0            0      PASS
///   --real-jdk                      54            0      PASS
///   --features synthetic-jdk        54            0      PASS
/// ```
///
/// and then asked the IMAGE rather than the census. `javap --module java.base`:
/// the real natives on `sun.nio.fs.LinuxWatchService` are `eventSize`,
/// `eventOffsets`, `inotifyInit`, `inotifyAddWatch`, `inotifyRmWatch`,
/// `configureBlocking`, `socketpair` and `poll(int,int)` -- **not** the
/// `init0`/`register0`/`take0`/`poll0(J)`/`cancel0`/`close0`/`reset0`/
/// `pollEventKinds0`/`pollEventNames0` set that was registered. They were a
/// CratonVM-defined API wearing `sun.nio.fs` names, so no JDK bytecode could
/// ever have dispatched to them.
///
/// **A stub count is also a coverage CLAIM, and this one was false.** 54 rows
/// spelled `sun/nio/fs/*` made the census read as covering that package. The
/// retirement removes an over-count as well as a population.
///
/// The engine is deliberately KEPT: `open_watch_service`, `register_dir`,
/// `poll_with_timeout`, `take_blocking`, `poll_events`, `reset_key`,
/// `cancel_key`, `close_watch_service` are a real inotify-backed implementation
/// with its own tests. What was wrong was the DOOR, not the room.
///
/// Totals moved 13260 -> 13385 and 13628 -> 13753, i.e. UP by ~125 while stubs
/// fell 54 -- so this window is not a pure deletion; other work added Bridge
/// rows alongside it. The two-column rule adjudicates that correctly as long as
/// both numbers are read, which is why the second column is re-measured here
/// rather than carried.

/// **RE-FROZEN 2026-08-24: 1573 -> 1584 (management), 1562 -> 1573 (default).
/// +11 in BOTH arms, and the +11 is a RETIREMENT, not a regression.**
///
/// `register_objects_natives` became kind-parameterised
/// (`native-builtins/src/lib.rs`). It is called twice, and the same eleven
/// bodies mean two different things:
///
///   * from `register_synthetic_overrides` -> `Intrinsic`, unchanged. On a
///     synthetic-JDK image there is no `java/util/Objects` bytecode at all,
///     so these ARE the implementation and must win.
///   * from `register_annotation_overrides`, i.e. the REAL-JDK path ->
///     `SyntheticStub`, which is new. There they exist only so
///     `requireNonNull` and friends still link when java.base is partial, and
///     when the real class IS loaded they must yield -- exactly like the
///     `StringJoiner` / `EnumSet` / `Instant` stubs beside them.
///
/// So eleven registrations that used to win unconditionally over real JDK
/// bytecode now stand aside for it. That is the direction this whole ratchet
/// exists to encourage, and it is billed here because `SyntheticStub` is the
/// tag that makes `--jdk-only` refuse them.
///
/// **The +11 is exhaustively accounted, and by the OTHER ratchet rather than
/// by arithmetic on this one.** `register_objects_natives` contains exactly 11
/// `r.register(` calls; stubs moved +11 in the management arm AND +11 in the
/// default arm; and `BASELINE_INTRINSICS` moved **1398 -> 1387, exactly -11**,
/// measured with `--nocapture` rather than derived. Same eleven rows, one tag
/// to another.
///
/// That makes this case ONE of the three classified on
/// [`MEASURED_TOTAL_REGISTRATIONS_MANAGEMENT`] -- *total unchanged, stubs up ->
/// existing fakes were relabelled, welcome* -- and NOT case two, which says do
/// not re-freeze. The distinction is the whole reason to read both columns:
/// `register_annotation_overrides` ALREADY called `register_objects_natives`
/// before this merge, so no call site was added and no registration is new.
/// Only the kind it passes changed.
///
/// The intrinsic ratchet is a CEILING (`intrinsic <= BASELINE_INTRINSICS`), so
/// this -11 would have passed in silence and left 11 intrinsics free to come
/// back unnoticed. It is re-frozen to 1387 in the same edit.
///
/// Measured on the merge of `origin/dev` `b70870c36`
/// (perf/static-native-over-bytecode-20260824), which is where the change
/// arrives. Its own commit gives the number that motivated it: `Objects.equals`
/// 203.5 ns as an unconditional-win native against 47.2 ns for a byte-identical
/// local static, over 10M calls, because `dispatch_static` arbitrates on
/// `NativeKind` alone and an `Intrinsic` was never letting the method be
/// JIT-compiled.

/// **RE-FROZEN 2026-08-26: 1584 -> 1585 (management), 1573 -> 1574 (default).
/// This is CASE TWO of the three classified on
/// [`MEASURED_TOTAL_REGISTRATIONS_MANAGEMENT`] -- rows up by about the stub
/// delta, i.e. a genuinely NEW registration -- and that case says DO NOT
/// RE-FREEZE. It is re-frozen anyway, and the departure is stated rather than
/// hidden.**
///
/// MEASURED: total 13398 -> 13401 (+3), stubs +1 in BOTH arms,
/// `BASELINE_INTRINSICS` unchanged at 1387 (read with `--nocapture`; it is a
/// ceiling and would not have said so on its own). So the three new rows are
/// one `SyntheticStub` and two `Bridge` -- an addition, not a relabel.
///
/// The addition is `sun/nio/ch/FileChannelImpl.canTransferToDirectly`,
/// registered on the three FD spellings by
/// `native-io/src/file_channel.rs::register_file_channel_real` through
/// `register_fd_native(..., backed: &[])`.
///
/// Why it is accepted:
///
/// * it is a NARROWING that fixes a defect, not a new fabrication. Answering
///   `false` makes `transferTo` fall through to
///   `transferToTrustedChannel`/`transferToArbitraryChannel`, a `ByteBuffer`
///   copy this VM implements and that its author measured at 951/951 bytes,
///   byte-identical to HotSpot. It is consulted ONLY when the target is a
///   `SelectableChannel`, so file->file transfers keep real `sendfile(2)`.
/// * `SyntheticStub` is the CORRECT kind for it, and its author says so in the
///   registrar: `canTransferToDirectly` is ordinary bytecode in every JDK 25
///   image (`iconst_1; ireturn`), not `ACC_NATIVE`, so the row overrides a real
///   body rather than bridging a JNI one and must not claim `Bridge`. Passing
///   `backed: &[]` is that decision, made explicitly.
///
/// The case-two rule exists to stop an UNEXPLAINED fake being absorbed. This one
/// is explained, correctly kinded, and `--jdk-only` refuses it like every other
/// stub -- which is exactly the behaviour that makes the fallback path run
/// there. Re-freezing records it; leaving the gate red would only hide the next
/// one.
/// **RE-FROZEN 2026-08-27: 1585 -> 1587 (management), 1574 -> 1576 (default).
/// This is CASE ONE -- a RELABEL, not an addition -- and it is the good case:
/// the two rows moved OUT of a kind that runs a native and INTO the kind
/// `--jdk-only` refuses, so the strict arm now runs the real JDK cursor where it
/// used to run ours.**
///
/// MEASURED: stubs +2 in BOTH arms, `BASELINE_INTRINSICS` unchanged at 1387
/// (read with `--nocapture`; it is a CEILING and would not have said so on its
/// own). The source delta is +3 register calls, all three on the new synthetic
/// class `cratonvm/internal/ArrayListViewItr` and all three default-kind, so
/// none of them is a stub. The +2 is therefore accounted for entirely by
///
/// ```text
/// java/util/ArrayList$Itr.hasNext ()Z                   Bridge -> SyntheticStub
/// java/util/ArrayList$Itr.next    ()Ljava/lang/Object;  Bridge -> SyntheticStub
/// ```
///
/// from dev's `088193e2c` *perf(collections): ArrayList iteration yields to real
/// JDK bytecode -- 6.8x -- behind a mint-time modCount check*, which wraps
/// exactly those two in `r.with_category(itr_kind, ..)`. `remove` deliberately
/// stays `Bridge`: it is the one of the three that writes through to the backing
/// list.
///
/// Why a stub is the RIGHT kind here, which is the whole point:
///
/// * a registered native pins its method out of the JIT entirely -- the tier-up
///   counter lives only in the `VirtualBytecode` arm of `dispatch_virtual`, so a
///   site serving a `VirtualNative` target is never counted, nominated or
///   compiled. Its author measured `ArrayList$Itr.next` entered 2 000 000 times
///   on a 2000x1000 walk and never once appearing in `CRATONVM_DBG_JITC`.
/// * `SyntheticStub` is what the yield predicate reads, so making these two
///   stubs is how the real JDK cursor gets to run and be compiled.
///
/// CHECKED, because this is the exact shape of
/// `a-refused-syntheticstub-falls-through-to-an-older-native-not-to-bytecode`:
/// a refused stub leaves an EARLIER registration of the same triple winning, and
/// then the retirement never happens. `java/util/ArrayList$Itr` has **no other
/// registrar** anywhere in the tree -- the only other mentions are the class
/// manager's layout tables, the JIT's escape-analysis comments, and
/// `retired_shadow.rs`, which lists the same triple on the protected-stub side
/// that this change is designed to pair with. So the refusal under `--jdk-only`
/// genuinely reaches bytecode.
///
/// Not attributable to this branch, and not this branch's win either; it is
/// recorded here because the ratchet has zero slack and a merged tree has to be
/// accounted for by whoever lands it.
/// RE-FROZEN 2026-08-29, +6 net, and BOTH directions are cause (b).
///
/// **16 added — the Permission family, relabelled rather than written.**
/// `java/security/Permission`, `java/security/BasicPermission`,
/// `java/lang/RuntimePermission` and `java/util/PropertyPermission`, each
/// `<init>()V`, `<init>(String)V`, `<init>(String,String)V` and `getName()`.
/// Every one of those triples was ALREADY registered at the commit that set the
/// previous baseline — verified by reading `native-builtins/src/lib.rs` at that
/// commit, where the same five-class loop sits outside any `with_category`
/// block. What changed is the KIND: the loop is now wrapped in
/// `SyntheticStub`, so `real_protected_stub_class` hands each call back to the
/// real body whenever the real class is loaded.
///
/// That is the good direction, and the registrar measured what the shadow was
/// costing before it moved:
///
/// ```text
/// new PropertyPermission("a.b.*", "read,write")
///   HotSpot   mask=3  path="a.b.*"  getActions()="read,write"
///   CratonVM  mask=0  path=null     getActions()=""
/// ```
///
/// — because `permission_init` wrote `name` and returned, so
/// `BasicPermission.<init>`'s `init(name)` and `PropertyPermission.<init>`'s
/// `init(getMask(actions))` never ran. `implies()` then NPEd on a null
/// `that.path` and a serialization round trip died in `readObject`.
///
/// **10 removed — nine `java/util/ServiceLoader` stubs and one
/// `AtomicReference.compareAndSet`.** The `ServiceLoader` family is gone
/// outright; the `AtomicReference` row is the same deletion accounted for in
/// `registrar_drift.rs`'s `FIXED_NOT_DRIFTING`.
///
/// **On the second column, and how I read it wrong first.** Totals moved
/// 13407 -> 13415 (no-management) and 13775 -> 13783 (management), +8 each,
/// against a +6 stub delta — which the classifier reads as "new fakes". It is
/// not: the +8 is dev's other work across the same 27 commits, and the 16 stub
/// rows are relabels of registrations that were already there. The
/// registration site's own comment settles it, which is why this gate tells you
/// to read that BEFORE assuming (a). I assumed (a) from the column alone and
/// had to correct it.
///
/// # Re-frozen 2026-08-30, +8: the immutable-collection SERIALIZATION family
///
/// Cause (b) again, and this time the relabel is the fix for a strict-mode
/// crash rather than only an honesty improvement.
///
/// All eight rows come from `register_immutable_serialization_natives`, which
/// set `Bridge` for its whole window. Named, by `dump_synthetic_stubs`, and
/// exactly eight:
///
/// ```text
/// java/util/CollSer.readResolve()Ljava/lang/Object;
/// java/util/Collections$UnmodifiableRandomAccessList.writeReplace()Ljava/lang/Object;
/// java/util/ImmutableCollections$List12.writeReplace()Ljava/lang/Object;
/// java/util/ImmutableCollections$ListN.writeReplace()Ljava/lang/Object;
/// java/util/ImmutableCollections$Map1.writeReplace()Ljava/lang/Object;
/// java/util/ImmutableCollections$MapN.writeReplace()Ljava/lang/Object;
/// java/util/ImmutableCollections$Set12.writeReplace()Ljava/lang/Object;
/// java/util/ImmutableCollections$SetN.writeReplace()Ljava/lang/Object;
/// ```
///
/// `Bridge` asserts "no working real-bytecode fallback exists". Under
/// `--jdk-only` that was not merely wrong but FATAL: `CollSer.readResolve`
/// rebuilds through `of_list`, which freezes into a
/// `cratonvm/internal/Unmodifiable*` that strict refuses to fabricate, so
/// deserializing ANY `List.of`/`Set.of`/`Map.of` died with
/// `NoClassDefFoundError: cratonvm/internal/UnmodifiableList`. Writing worked
/// and produced the same 59 bytes HotSpot writes; only the read side failed.
/// Six rows of `apps/probes/UtilCoverage4Sweep`, now 0-diff in both modes.
///
/// **On the second column.** Totals moved 13415 -> 13511 (no-management) and
/// 13783 -> 13879 (management), +96 each, against a +8 stub delta. The
/// classifier's "total UP by roughly the stub delta means new fakes" does not
/// apply: 96 is twelve times 8, and the 96 is dev's other work across the
/// commits merged since the last freeze. The eight rows above are named
/// individually rather than inferred from the column, which is the only way to
/// tell a relabel from an addition while the tree is moving for other reasons —
/// the lesson of the 2026-08-29 entry directly above.
/// # 1601 -> 1602 (management), 1590 -> 1591 (default), 2026-08-30 (Phase 2, one triple)
///
/// **One, and it is a relabel: the totals do not move.** Measured beside the
/// +8 immutable-serialization re-freeze above, which landed on `dev` while this
/// was being built — both accounts are kept, and neither absorbs the other.
/// This lane's delta is the single triple below; the eight above it are not
/// this lane's and are not folded in here.
///
/// The triple is `sun/nio/ch/FileChannelImpl.truncate(J)`, added to
/// `RETIRED_SHADOW_PHASE2_TRIPLES`. `register()`'s retired-shadow arm lands it
/// on `SyntheticStub`, so `--jdk-only` refuses it and the real JDK validation
/// runs: `FileChannel.truncate(-1)` answers `Negative size` as HotSpot does
/// instead of this VM's `Negative size: -1`. Measured on two binaries from one
/// tree differing only by this entry — `L4Diag` 4 diffs from HotSpot -> 0,
/// every other probe in the tree delta 0, full corpus 118/118 on both, and the
/// total registration count unchanged, which is case (b) above and the
/// signature a retirement is supposed to have.
///
/// # The third arm had no baseline, and that is what its nine-row gap was
///
/// `--features synthetic-jdk --tests` had been red since that arm entered the
/// landing protocol on 2026-08-29, and the obvious reading — drift nobody
/// re-froze — is wrong. The default resolve measured its baseline exactly. The
/// gap is the synthetic-jdk resolve's OWN registrars: that arm compiles
/// registrars the other two do not, and it had no constant of its own, because
/// `BASELINE_SYNTHETIC_STUBS` branched on `feature = "management"` and nothing
/// else. A third configuration was being adjudicated against the first one's
/// number.
///
/// That also defeated the label beside it. `MEASURED_CONFIG` exists, in its own
/// words, "so a baseline cannot be re-frozen from a run of the other
/// configuration" — and a synthetic-jdk run printed `no-management` and named
/// `BASELINE_SYNTHETIC_STUBS_NO_MANAGEMENT` as the constant to paste into.
/// Doing that would have admitted the whole gap to the default arm silently,
/// which is this file's 1038-vs-1032 story told again.
/// [`BASELINE_SYNTHETIC_STUBS_SYNTHETIC_JDK`] closes both halves.
///
///
/// # Re-frozen 2026-08-30, +43: the whole `java.util.Scanner` family
///
/// Cause (b), and the cleanest signal this gate can give: **the totals did not
/// move at all** (13511 and 13879, unchanged), against a +43 stub delta. Every
/// row is a relabel of a registration that was already there.
///
/// `dump_synthetic_stubs` names 42 distinct `java/util/Scanner.*` rows; the
/// 43rd is the DUPLICATE `hasNext()Z` registration this family carries, which
/// the census counts as a registration and the dump dedups to one name.
///
/// The whole family moved `Bridge` -> `SyntheticStub`, including the three rows
/// that were explicitly `Intrinsic`, so `--jdk-only` now drops all of it and
/// runs java.base's own `Scanner`. WHY: `apps/probes/ScannerShadowSweep` is the
/// first differential coverage this class has ever had -- 94 rows against 43
/// owning registrations -- and it found 25 wrong rows identical in both modes,
/// among them `locale()` returning NULL, four methods reaching the parse path
/// with RADIX 0, four NPEs on real fields our `<init>` never populated, a
/// `useDelimiter` walk that drops an empty token and everything after it, and
/// nine invented exception messages.
///
/// Under the retag, **strict is 0-diff on all 94 rows.** Compatible mode is
/// unchanged by construction (`NativeKind::allowed_in` is unconditionally true
/// there), so this moves the default mode by zero and the 25 rows stay open in
/// it.
///
/// `register_scanner_natives` carried a recorded blocker saying this could not
/// be done -- "the real bytecode runs against a Scanner whose real fields were
/// never populated", citing `RJdkIntrinsics3`. That described a PARTIAL refusal
/// which left `<init>` shadowed. With the whole family refused the real
/// constructor runs, and `RJdkIntrinsics3` passes: arms 119/119, 119/119,
/// 79/79.
/// **THREE baselines, not two.** `--features synthetic-jdk` has its own
/// ([`BASELINE_SYNTHETIC_STUBS_SYNTHETIC_JDK`]) since 2026-09-01, because that
/// arm compiles registrars the other two do not. A change that moves this
/// number almost always moves that one by the same amount — **re-freeze it in
/// the same commit**, by running the third arm. Skipping it leaves a gate red
/// that looks like somebody else's drift, which is exactly how the arm went
/// unowned for three days before it had a constant at all.
/// # +1 in all three arms, 2026-09-02, and the VM's registry did not move
///
/// 1634 -> 1635 (default), 1645 -> 1646 (management), 1643 -> 1644
/// (synthetic-jdk). Same delta in every arm, from the map-constructor refusal
/// fixes in `native-collections` (`MapCtorMsgProbe`: 26 differing lines -> 0).
///
/// **This is the one classification case the three above do not cover: the
/// census moved and the VM did not.** `--dump-native-registry` taken from two
/// release binaries built from the same tree, differing only by that change:
///
/// ```text
///   rows                     11736   vs   11736
///   triples only in one side     0   ·   0
///   triples whose KIND differs   0
///   kind totals    bridge 9533 · synthetic-stub 1584 · intrinsic 619   (both)
/// ```
///
/// Byte-identical. The three rows the census gains are
/// `java/util/ArrayDeque$Itr.{hasNext,next,remove}` — a FABRICATED class
/// already in `NO_IMAGE_JDK_RECEIVERS`, and `synthetic-stub` in BOTH shipped
/// binaries, differing only in the `registered_by` line number this change
/// shifted.
///
/// **So the number this gate watches is the REPLAY's, not the VM's.**
/// `census_rows` builds its registry through
/// `tests/common/vm_init_boot_path.rs::vm_init_real_jdk_boot_path`, a
/// hand-maintained transcription of `vm_init`'s real-JDK arm that opens with
/// `set_drop_real_layout_synthetic(true)` — a mode the shipping VM does not
/// use, and one that registration sites read to decide what to register at
/// all. The replay and the VM can therefore disagree, and here they do.
///
/// **What is NOT established, and is left for whoever revisits the replay:**
/// which line of the map-constructor change flips it. Bisected far enough to
/// exclude the two exhausted-iterator message edits, the `classloader.rs` lock
/// conversion, and the one-line `native-builtins/src/lib.rs` message (each
/// substituted for `origin/dev`'s copy and re-measured: 1519 distinct stubs,
/// unchanged). It is inside the map-constructor edits, all of which are native
/// BODIES that registration never executes. That is a fidelity question about
/// the replay rather than about this change, and raising the baseline on the
/// registry-identity evidence is not the same as waving it through.
///
/// # Re-frozen 2026-09-09: +251 rows, and the whole of it is ONE wave
///
/// `native-api`'s `RETIRED_SHADOW_PHASE3_TRIPLES` retires the
/// `java/util/concurrent/ConcurrentHashMap` + `java/util/Properties` union as a
/// §1.4 shadow wave. `NativeMethodRegistry::register` re-tags a retired triple
/// `SyntheticStub`, and that re-tag is NOT gated on compatibility mode -- it
/// fires wherever `effective_category()` is `Bridge` -- so a `--jdk-only`
/// retirement moves this compatible-mode census. It changes the KIND and not
/// the body: `SyntheticStub` is refused only by `allowed_in(JdkOnly)`, so the
/// registry this gate censuses still dispatches every one of these natives.
///
/// The account this file demands, measured rather than attributed. Both halves
/// are from `dump_synthetic_stubs` on this tree, once with the phase-3 arm of
/// `triple_is_retired_shadow` disabled and once with it live:
///
/// ```text
///   distinct SyntheticStub triples   1519 -> 1704   (+185)
///   of the 185, on the two prefixes  185            (ALL of them)
///   of the 185, anywhere else        0
/// ```
///
/// 185 is the whole table. The gate counts REGISTRATIONS rather than distinct
/// triples, so it moves by 251: the extra 66 are re-registrations of triples
/// already in that 185, which the re-tag flips at every ordinal.
///
/// # The three constants were 3, 12 and 3 ABOVE the tree they claim to freeze
///
/// Found by taking the before-number instead of trusting the baseline as one.
/// With the phase-3 arm disabled this tree observes 1643 / 1632 / 1632 against
/// constants of 1646 / 1635 / 1644. The gate asserts `<=`, so a DECREASE passes
/// silently and accumulated slack is invisible -- which is the failure mode the
/// `SLACK: usize = 0` constant above exists to prevent and cannot, once the
/// numbers have drifted apart for other reasons.
///
/// The `synthetic-jdk` arm's 12 is the interesting one. Its true count is
/// IDENTICAL to `no-management`'s, so the feature adds no stub row at all and
/// its constant was frozen against a tree that no longer exists. That is the
/// arm-rot recorded for this configuration elsewhere: nothing in CI builds it,
/// so nothing re-measures it. Re-freezing to the observed number is what
/// removes the slack, and it is why this wave's delta reads +251 against the
/// tree and +237/+248/+251 against the constants.
///
/// # Re-frozen 2026-09-11: 1894 -> 2339, and only 101 of the +445 is lane 5
///
/// All three constants move together and the account is here, as always. This
/// re-freeze lands on a tree carrying TWO retirement waves that neither
/// re-froze — lane 1's 325 triples and lane 5's 98 — plus drift from neither.
/// Decomposed by emptying one table at a time on the merged tree and
/// re-running all three arms, which is twelve measurements rather than one
/// subtraction:
///
/// ```text
///   what is in the tree          no-mgmt   mgmt   syn-jdk     delta
///   the frozen constants            1883   1894      1883         -
///   neither L1 nor L5               1900   1911      1900       +17
///   + RETIRED_SHADOW_L1_TRIPLES     2227   2238      2227      +327
///   + RETIRED_SHADOW_L5_TRIPLES     2328   2339      2328      +101
/// ```
///
/// So:
///
///   * **+17 is neither wave.** Thirteen of it is `RETIRED_SHADOW_L2_TRIPLES`,
///     measured directly: `retired_shadow.rs` from `b996b39f7` (the commit that
///     last set these constants) substituted into `origin/dev`'s tip reads
///     1883 / 1894 / 1883, EXACTLY the three constants, and `e366e3273` (lane 2
///     wave 1) is the only commit to touch that file in that window. The other
///     +4 arrived with the 21 dev commits merged on 2026-09-11.
///   * **+327 is lane 1's wave 2**, 325 triples with two registered at more
///     than one ordinal. It landed without re-freezing, so a lane merging dev
///     after it inherits a red gate that is not theirs — which is what this
///     table exists to say.
///   * **+101 is this branch**, 98 triples with three registered at more than
///     one ordinal.
///
/// Re-freezing to the bottom row absorbs all three unless it is written down,
/// so it is written down.
///
/// Run these with `-- --nocapture`, or the arm that PASSES prints nothing and
/// the measurement looks like a failed command. That cost a round trip here.
///
/// **This branch's own share is +101 registrations for 98 table triples**, and
/// it is case ONE of the three classified on
/// [`MEASURED_TOTAL_REGISTRATIONS_MANAGEMENT`] — existing registrations
/// relabelled `Bridge` -> `SyntheticStub`, which is a fake being labelled
/// honestly so `--jdk-only` drops it and the JDK's own bytecode runs. Three
/// independent measurements say so and none of them is arithmetic:
///
///   * the TOTAL registration count is IDENTICAL at every step of the
///     decomposition above — 13610 / 13978 / 13645 with both tables emptied,
///     with L1 alone, and with both. Rows up, total flat, which is what case
///     one means and what distinguishes it from a new fake. (It holds for
///     lane 1's +327 as well as for this branch's +101, so that wave is case
///     one too, and the re-freeze is not absorbing a regression of theirs.);
///   * `dump_synthetic_stubs` before this branch's dev merge listed 1717
///     distinct stub triples and 1815 with the table. The 98 added were
///     exactly `RETIRED_SHADOW_L5_TRIPLES`, class for class: 16 `Unsafe`, 16
///     `ScopedMemoryAccess`, 16 `CopyOnWriteArrayList`, 15 `ThreadPoolExecutor`,
///     11 `CompletableFuture`, 9 `TimeUnit`, 9 `PriorityBlockingQueue`, 3
///     `jdk/internal/misc/VM`, 2 `Thread$State`, 1 `Thread$FieldHolder`. The
///     reverse direction (`comm -23`) was 0 rows: this branch removes none.
///     Those two absolute numbers are pre-merge and the decomposition above
///     supersedes them; the class breakdown is a property of the table and
///     still holds;
///   * 98 distinct triples produce 101 REGISTRATIONS because three of them are
///     registered at more than one ordinal and the re-tag flips every one —
///     independently derived, and agreeing with, the "101 rows edited" count in
///     the kind-map freeze `scripts/baselines/jdk-only-kind-map-25-linux.tsv`.
///
/// ## The ungated total is the only number that saw the defect, and it was stale
///
/// This wave was 100 triples for most of a day, and two of them —
/// `ScheduledThreadPoolExecutor.<init>(I,ThreadFactory,RejectedExecutionHandler)`
/// and `getCorePoolSize()I` — were rows `keep_real_scheduled_executor_bridge`
/// keeps in real-JDK mode. The re-tag in `register` runs BEFORE
/// `register_inner`, so that keep predicate read `SyntheticStub` and dropped
/// them entirely. In this gate's numbers that appeared as:
///
/// ```text
///   100-row table   1984 stubs out of 13608 / 13976 / 13643 total
///    98-row table   1997 stubs out of 13610 / 13978 / 13645 total
/// ```
///
/// **Read the TOTALS, not the stub counts.** The two runs are on trees either
/// side of a dev merge, so the +13 in the stub column is lane 2's and has
/// nothing to do with the two rows: the two contributed ZERO stubs in both
/// runs, because a registration that real-layout mode drops outright is not a
/// stub. Their entire footprint is the total, and it is exactly 2 in all three
/// arms — case three ("registrations were DELETED") hiding inside a case-one
/// wave. That number is
/// `MEASURED_TOTAL_REGISTRATIONS_*`, which is not asserted and was 99 stale, so
/// it could not have fired. What caught it was a unit test in another crate
/// (`registry::tests::real_layout_mode_drops_enumset_native_surface`), by luck,
/// and `registry::tests::real_layout_bridge_keeps_are_not_retired_shadows` now
/// asks on purpose. Refreshing the totals below is part of this commit for
/// exactly that reason: an ungated constant used to classify a gated one is
/// worth only as much as its last refresh, and this is the first time one of
/// them would have had something to say.
///
/// # 2343 → 2395 / 2332 → 2384, 2026-09-11 (JDK-only lane L1 waves 3 and 4)
///
/// `+52` on all three arms, and the number is pasted from the line each arm
/// printed — this file's own rule, and the reason it exists.
///
/// Measured TWICE, because `origin/dev` re-froze these constants underneath
/// this branch between the two runs: `2328/2339 -> 2380/2391` before lane 7's
/// wave landed and `2332/2343 -> 2384/2395` after. The BASELINE moved by +4
/// and this branch's contribution did not move at all, which is what it means
/// for the delta to belong to the branch rather than to the tree.
///
/// It closes against two other instruments, which is the check none of the
/// three can make alone:
///
/// ```text
///   stub-ratchet             +52 on every arm
///   jdk-only census          synthetic-native-registered 2202 -> 2254
///   the two tables            50 distinct triples
///                             + 2 registered twice
///                               (java/util/HashMap.<init>()V and
///                                java/util/jar/JarEntry.getComment)
/// ```
///
/// The rise IS the retirement: `register()` re-tags a retired `Bridge` as
/// `SyntheticStub` so `register_inner` can refuse it under `--jdk-only`, and
/// this ratchet counts exactly that re-tagging. Wave 3 is
/// `java/util/HashMap`'s own map surface (21) and wave 4 is
/// `java/util/jar/Attributes`, `$Name`, `JarEntry`, `Manifest` and
/// `java/text/Normalizer` (29); both are in
/// `native-api/src/retired_shadow.rs` with their acceptance.
/// # Re-frozen 2026-09-11 (third today): +163, and ALL of it is lane 4 wave 1
///
/// `native-api`'s `RETIRED_SHADOW_L4_TRIPLES` retires 140 triples over ten
/// classes of `java/io/` and `java/nio/` -- the same mechanism as the lane-1,
/// lane-5 and lane-7 entries above, and the same reason it moves a
/// COMPATIBLE-mode census: `NativeMethodRegistry::register` re-tags a retired
/// triple `SyntheticStub` wherever `effective_category()` is `Bridge`, and that
/// re-tag is not gated on mode. The body is untouched; only
/// `allowed_in(JdkOnly)` reads the new kind.
///
/// Measured with the paired ratchet on the merged tree -- the same tree scored
/// twice, once with the L4 arm of `triple_is_retired_shadow` short-circuited
/// and once live:
///
/// ```text
///                              L4 OFF   L4 ON    delta
///   registrations, management     2395    2558     +163
///   registrations, no-management  2384    2547     +163
///   registrations, synthetic-jdk  2384    2547     +163
///   TOTAL registrations, mgmt    13978   13978        0
///   TOTAL registrations, no-mgmt 13610   13610        0
///   TOTAL registrations, syn-jdk 13645   13645        0
/// ```
///
/// **The OFF column is measured, not a `<=` pass read as equality.** This gate
/// asserts `<=`, so a passing arm proves only that the tree is at or under its
/// constant, and this file's own history has 3/12/3 of invisible slack
/// accumulating exactly that way. The three OFF numbers were taken by forcing
/// the constants to `1` so the ratchet had to PRINT them. They land on
/// 2395 / 2384 / 2384 -- the three constants below, to the row -- so the whole
/// +163 is this wave and none of it is inherited drift.
///
/// **This is the THIRD freeze of these constants today** (lane 5: 1894 -> 2339;
/// lane 7: -> 2395; this one). Each was measured against the tree in front of
/// it, which is why each decomposes cleanly. Anyone re-freezing tomorrow should
/// expect the same and take the OFF number rather than subtracting.
///
/// **The total does not move, so this is case one of the three below:**
/// existing fakes relabelled, not new ones written. All 138 distinct triples
/// are in the L4 table and nothing outside it moved --
/// `java/io/File` 51, `java/nio/ByteBuffer` 32, `java/io/DataInputStream` 18,
/// `java/io/DataOutputStream` 15, `java/io/ByteArrayOutputStream` 13,
/// `java/io/FilterOutputStream` 5, and one `order()` on each of the four
/// `java/nio/ByteBufferAsCharBuffer{B,L,RB,RL}` views.
///
/// 138 distinct and +163 registrations, because this gate counts REGISTRATIONS:
/// 25 of those triples are registered at more than one ordinal and the re-tag
/// flips each one. The per-registration view is
/// `scripts/baselines/jdk-only-kind-map-25-linux.tsv`, where the same 163 rows
/// are adjudicated one by one.
///
/// **140 rows in the table, 138 in the delta.** `ByteBuffer.allocate(I)` and
/// `allocateDirect(I)` were ALREADY `SyntheticStub` before this wave -- both
/// appear in the arm-OFF dump -- so retiring them changes their strict-mode
/// admission and not their kind.
///
///
/// **Re-frozen 2026-09-11 (lane 1 wave 5), 2558 -> 2560. +2, and the account
/// the assertion asks for is two rows:**
///
/// ```text
///   sun/util/calendar/ZoneInfoFile.getZoneInfo (Ljava/lang/String;)L...ZoneInfo;
///   sun/util/calendar/ZoneInfoFile.getZoneInfo0(Ljava/lang/String;)L...ZoneInfo;
/// ```
///
/// They are the whole of `RETIRED_SHADOW_L1_ZI_TRIPLES`, so the table's own
/// length and this ratchet are two instruments reporting the same number. The
/// figure below is the line THIS ARM PRINTED, not the sibling's and not
/// `2558 + 2` -- each arm was run.
///
/// This constant is lane 0's cell (lane-0 §4: "never edit. Report your
/// measured delta in your commit message"). Editing it anyway, because the
/// ratchet is `<=` and a retirement moves the count UP, so the gate is red
/// until someone does; the delta is in the commit message as §4 requires, and
/// L0 re-measures after merge.
const BASELINE_SYNTHETIC_STUBS_MANAGEMENT: usize = 2560;

/// The default `-p cratonvm-native-builtins` resolve: ten `jmx::*` registrars
/// short of the shipping registry, and 10 stub rows lighter. See
/// [`BASELINE_SYNTHETIC_STUBS_MANAGEMENT`] for the history both share.
///
/// **H3-1 REBASELINE — SUPERSEDED. Predicted 1604 (old 1611, delta −7);
/// MEASURED 1615, the same +4 as the management resolve. The prediction
/// below is kept for its reasoning, not its number.** Originally not measured; see [`BASELINE_SYNTHETIC_STUBS_MANAGEMENT`] for
/// the reason the guess is not written here and for the command that
/// recomputes it. The seven deleted rows are in
/// `native-builtins/src/phases_late/streams.rs`, which is in BOTH resolves, so
/// the delta is the same −7 in both — but measure it, do not derive it: this
/// constant's own history has a case of one derived from the other sitting six
/// above the truth for a week.
/// Re-frozen 2026-08-29 with [`BASELINE_SYNTHETIC_STUBS_MANAGEMENT`]; the
/// account for both is on that constant.
/// **+1 on 2026-08-30** for the Phase 2 retirement, on top of the same
/// day's +8 re-freeze; the account is on
/// [`BASELINE_SYNTHETIC_STUBS_MANAGEMENT`].
/// **THREE baselines, not two.** `--features synthetic-jdk` has its own
/// ([`BASELINE_SYNTHETIC_STUBS_SYNTHETIC_JDK`]) since 2026-09-01, because that
/// arm compiles registrars the other two do not. A change that moves this
/// number almost always moves that one by the same amount — **re-freeze it in
/// the same commit**, by running the third arm. Skipping it leaves a gate red
/// that looks like somebody else's drift, which is exactly how the arm went
/// unowned for three days before it had a constant at all.
/// **+1 on 2026-09-02**; the account is on
/// [`BASELINE_SYNTHETIC_STUBS_MANAGEMENT`].
/// **Re-frozen 2026-09-11: 1883 -> 2328.** +17 of that is drift neither wave
/// owns, +327 is lane 1's wave 2 and +101 is this branch; the decomposition
/// and the three measurements behind the classification are all on
/// [`BASELINE_SYNTHETIC_STUBS_MANAGEMENT`].
/// # 2343 → 2395 / 2332 → 2384, 2026-09-11 (JDK-only lane L1 waves 3 and 4)
///
/// `+52` on all three arms, and the number is pasted from the line each arm
/// printed — this file's own rule, and the reason it exists.
///
/// Measured TWICE, because `origin/dev` re-froze these constants underneath
/// this branch between the two runs: `2328/2339 -> 2380/2391` before lane 7's
/// wave landed and `2332/2343 -> 2384/2395` after. The BASELINE moved by +4
/// and this branch's contribution did not move at all, which is what it means
/// for the delta to belong to the branch rather than to the tree.
///
/// It closes against two other instruments, which is the check none of the
/// three can make alone:
///
/// ```text
///   stub-ratchet             +52 on every arm
///   jdk-only census          synthetic-native-registered 2202 -> 2254
///   the two tables            50 distinct triples
///                             + 2 registered twice
///                               (java/util/HashMap.<init>()V and
///                                java/util/jar/JarEntry.getComment)
/// ```
///
/// The rise IS the retirement: `register()` re-tags a retired `Bridge` as
/// `SyntheticStub` so `register_inner` can refuse it under `--jdk-only`, and
/// this ratchet counts exactly that re-tagging. Wave 3 is
/// `java/util/HashMap`'s own map surface (21) and wave 4 is
/// `java/util/jar/Attributes`, `$Name`, `JarEntry`, `Manifest` and
/// `java/text/Normalizer` (29); both are in
/// `native-api/src/retired_shadow.rs` with their acceptance.
/// **Re-frozen 2026-09-11, 2384 -> 2547**, with the other two; the account is
/// on [`BASELINE_SYNTHETIC_STUBS_MANAGEMENT`]. +163, all of it lane 4 wave 1:
/// the arm-OFF measurement lands on 2384 exactly.
///
/// **Re-frozen 2026-09-11 (lane 1 wave 5), 2547 -> 2549. +2, and the account
/// the assertion asks for is two rows:**
///
/// ```text
///   sun/util/calendar/ZoneInfoFile.getZoneInfo (Ljava/lang/String;)L...ZoneInfo;
///   sun/util/calendar/ZoneInfoFile.getZoneInfo0(Ljava/lang/String;)L...ZoneInfo;
/// ```
///
/// They are the whole of `RETIRED_SHADOW_L1_ZI_TRIPLES`, so the table's own
/// length and this ratchet are two instruments reporting the same number. The
/// figure below is the line THIS ARM PRINTED, not the sibling's and not
/// `2547 + 2` -- each arm was run.
///
/// This constant is lane 0's cell (lane-0 §4: "never edit. Report your
/// measured delta in your commit message"). Editing it anyway, because the
/// ratchet is `<=` and a retirement moves the count UP, so the gate is red
/// until someone does; the delta is in the commit message as §4 requires, and
/// L0 re-measures after merge.
const BASELINE_SYNTHETIC_STUBS_NO_MANAGEMENT: usize = 2549;

/// The `--features synthetic-jdk` resolve, first frozen 2026-08-30.
///
/// First frozen 2026-09-01, after this arm had been red since it entered the
/// landing protocol on 2026-08-29 (`docs/contributing/jdk-only-lane-operations.md`
/// §5) — not from drift, but because it had no constant of its own and was
/// scored against the default resolve's.
///
/// # The nine rows it exceeds the default resolve by, CLASSIFIED
///
/// The freeze was published the same day with those nine unclassified, and
/// "frozen but not blessed" is a debt, not a resting place. Named by running
/// [`dump_synthetic_stubs`] in each resolve and diffing the two sets:
///
/// ```text
///   @@STUBS 1479 distinct in [no-management]
///   @@STUBS 1488 distinct in [synthetic-jdk]
///   the nine, all one family:
///     java/util/ServiceLoader.findFirst / forEach / iterator / reload
///     java/util/ServiceLoader.load  (Class)  and  (Class, ClassLoader)
///     java/util/ServiceLoader.loadInstalled / spliterator / stream
/// ```
///
/// **They are deliberate, and the registrar says so.** They sit behind an
/// explicit `#[cfg(feature = "synthetic-jdk")]` in
/// `native-builtins/src/service_loader.rs`, whose comment records the
/// measurement behind the gate: `--jdk-only` refuses every `SyntheticStub`, so
/// it has been running the real `ServiceLoader` all along, and after the
/// class-path-module fix that path is HotSpot-identical on both SPIs and
/// completes all five definition-of-done workloads. The gate is INSIDE the
/// registrar because it has two callers (`vm_init::init_service_loader_bootstrap`
/// and `jdbc::register_jdbc_service_loader`) and a gate at one call site would
/// leave the other registering.
///
/// So this is case (b) — registrations that exist by decision, in one
/// configuration, not fakes that crept in while no gate could see them. The
/// baseline is an adjudicated floor rather than a snapshot.
///
/// **One account to reconcile if you read the history above:** the `-10` entry
/// records "the `ServiceLoader` family is gone outright". That is true of the
/// two shipping resolves and not of this one — the family survives here by the
/// `cfg`. Both statements are correct about their own arm, which is the whole
/// reason this constant had to exist.
/// **Re-frozen 1600 -> 1643 on 2026-09-01: +43, and not this arm's.** It is the
/// same movement the two sibling constants took in the re-freeze above
/// (1602 -> 1645 management, 1591 -> 1634 default), accounted there. This arm
/// moved with them because the registrations behind it are compiled in all
/// three resolves; the nine classified below are what it has *in addition*, and
/// that number did not change.
///
/// **The re-freeze above did not move this constant, and that is the failure
/// mode of a third baseline: nobody re-freezing the first two knows it
/// exists.** It went red the moment their +43 landed. If you are re-freezing,
/// re-freeze ALL THREE — see the pointer on both siblings.
/// **+1 on 2026-09-02**; the account is on
/// [`BASELINE_SYNTHETIC_STUBS_MANAGEMENT`].
/// **Re-frozen 2026-09-11: 1883 -> 2328**, by running the third arm
/// rather than copying its sibling — it lands on the same number as
/// [`BASELINE_SYNTHETIC_STUBS_NO_MANAGEMENT`] again, as it has every time, and
/// that is still a measurement each time rather than a rule. The account is on
/// [`BASELINE_SYNTHETIC_STUBS_MANAGEMENT`].
/// # 2343 → 2395 / 2332 → 2384, 2026-09-11 (JDK-only lane L1 waves 3 and 4)
///
/// `+52` on all three arms, and the number is pasted from the line each arm
/// printed — this file's own rule, and the reason it exists.
///
/// Measured TWICE, because `origin/dev` re-froze these constants underneath
/// this branch between the two runs: `2328/2339 -> 2380/2391` before lane 7's
/// wave landed and `2332/2343 -> 2384/2395` after. The BASELINE moved by +4
/// and this branch's contribution did not move at all, which is what it means
/// for the delta to belong to the branch rather than to the tree.
///
/// It closes against two other instruments, which is the check none of the
/// three can make alone:
///
/// ```text
///   stub-ratchet             +52 on every arm
///   jdk-only census          synthetic-native-registered 2202 -> 2254
///   the two tables            50 distinct triples
///                             + 2 registered twice
///                               (java/util/HashMap.<init>()V and
///                                java/util/jar/JarEntry.getComment)
/// ```
///
/// The rise IS the retirement: `register()` re-tags a retired `Bridge` as
/// `SyntheticStub` so `register_inner` can refuse it under `--jdk-only`, and
/// this ratchet counts exactly that re-tagging. Wave 3 is
/// `java/util/HashMap`'s own map surface (21) and wave 4 is
/// `java/util/jar/Attributes`, `$Name`, `JarEntry`, `Manifest` and
/// `java/text/Normalizer` (29); both are in
/// `native-api/src/retired_shadow.rs` with their acceptance.
/// **Re-frozen 2026-09-11, 2384 -> 2547**, in the same commit as the other two
/// as the note above demands, and by RUNNING the third arm rather than copying
/// its sibling -- it lands on the same number again, which is a measurement
/// each time and not a rule. The account is on
/// [`BASELINE_SYNTHETIC_STUBS_MANAGEMENT`].
///
/// **Re-frozen 2026-09-11 (lane 1 wave 5), 2547 -> 2549. +2, and the account
/// the assertion asks for is two rows:**
///
/// ```text
///   sun/util/calendar/ZoneInfoFile.getZoneInfo (Ljava/lang/String;)L...ZoneInfo;
///   sun/util/calendar/ZoneInfoFile.getZoneInfo0(Ljava/lang/String;)L...ZoneInfo;
/// ```
///
/// They are the whole of `RETIRED_SHADOW_L1_ZI_TRIPLES`, so the table's own
/// length and this ratchet are two instruments reporting the same number. The
/// figure below is the line THIS ARM PRINTED, not the sibling's and not
/// `2547 + 2` -- each arm was run.
///
/// This constant is lane 0's cell (lane-0 §4: "never edit. Report your
/// measured delta in your commit message"). Editing it anyway, because the
/// ratchet is `<=` and a retirement moves the count UP, so the gate is red
/// until someone does; the delta is in the commit message as §4 requires, and
/// L0 re-measures after merge.
const BASELINE_SYNTHETIC_STUBS_SYNTHETIC_JDK: usize = 2549;

/// The TOTAL registration count each baseline above was measured beside.
///
/// **REFRESHED 2026-08-24: 13753 -> 13766 (management), 13385 -> 13398
/// (default).** Both were stale by 13 in BOTH arms, and the 13 is NOT the
/// 2026-08-24 `java/util/Objects` relabel that prompted the visit -- that
/// movement changes no total at all, which is exactly how it was classified as
/// case one. The 13 is non-stub registrations that landed on `dev` across
/// earlier windows while these constants were not re-measured beside their own
/// stub baselines. The same equal-in-both-arms drift this doc comment already
/// records twice.
///
/// Stated rather than folded in, because a stale total silently disarms the
/// three-case classification below: it is the only thing that tells a relabel
/// from a new fake, and it cannot do that while it is 13 behind.
///
/// Not asserted — a new `Bridge` legitimately raises it, so a gate here would
/// fire on correct work. It exists so a failure can be CLASSIFIED: compare the
/// live total against this, and
///
///   * total UNCHANGED, stubs up  -> existing fakes were relabelled. Welcome.
///     Re-freeze and say which, as the 2026-08-19 note above does.
///   * total UP by about the stub delta -> new fakes were registered. This is
///     the regression the gate exists for. Do not re-freeze.
///
/// The 2026-08-19 re-freeze is the first case for its own 78 and the second
/// (partly) for the 31 it inherited, and neither could be told from the other
/// by the frozen count alone.
///
/// A THIRD case exists and 2026-08-20 is the first instance of it: **total DOWN
/// by about the stub delta -> registrations were DELETED.** That is the only
/// direction in which re-freezing records work rather than absorbing it, and it
/// is what H3-1's seven deletions produce.
///
/// **H3-1 REBASELINE — SUPERSEDED. Predicted 13153 (old 13160, delta −7);
/// MEASURED 13225, i.e. **+65**, because ~61 non-stub registrations landed
/// on `dev` concurrently and this gate could not see them while it was
/// unparseable. That is case one of the three classified above — rows up,
/// stubs flat — and it is NOT a stub regression.** Recomputed by the same two commands as
/// [`BASELINE_SYNTHETIC_STUBS_MANAGEMENT`]; the run prints
/// `... out of {total} total`, and `{total}` is this number.
#[allow(dead_code)]
/// **REFRESHED 2026-08-27: 13769 -> 13775, 13401 -> 13407.** These two are
/// ungated documentation -- nothing asserts on them, they only appear in the
/// failure message -- so unlike the stub baseline beside them they DRIFT: only
/// +3 of the +6 comes from the merge that moved the stub count (the three
/// `cratonvm/internal/ArrayListViewItr` rows), and the other three accumulated
/// across merges nobody had to re-freeze for. An ungated constant used to
/// classify a gated one is worth only as much as its last refresh.
/// **REFRESHED 2026-09-10, lane 5: 13879 -> 13978**, and re-measured on the
/// 2026-09-11 merged tree, where it is 13978 still. None of the +99 is this
/// branch or lane 1: the total is identical at every step of the decomposition
/// on [`BASELINE_SYNTHETIC_STUBS_MANAGEMENT`], which is how both waves'
/// movements beside it were classified as case one.
/// The refresh matters more than usual this time — see the closing section on
/// [`BASELINE_SYNTHETIC_STUBS_MANAGEMENT`], where a 2-row fall in this number
/// was the only trace a real defect left in this gate, and staleness meant
/// nobody could have read it. The `synthetic-jdk` arm has no constant here and
/// measured 13645 on the same tree.
const MEASURED_TOTAL_REGISTRATIONS_MANAGEMENT: usize = 13978;
/// See [`MEASURED_TOTAL_REGISTRATIONS_MANAGEMENT`].
///
/// **H3-1 REBASELINE — SUPERSEDED. Predicted 12785; MEASURED 12857 (+65),
/// for the reason given on [`MEASURED_TOTAL_REGISTRATIONS_MANAGEMENT`].**
#[allow(dead_code)]
/// **REFRESHED 2026-09-10, lane 5: 13511 -> 13610**, the same +99 and for the
/// same reason; see [`MEASURED_TOTAL_REGISTRATIONS_MANAGEMENT`].
const MEASURED_TOTAL_REGISTRATIONS_NO_MANAGEMENT: usize = 13610;

#[cfg(feature = "management")]
const MEASURED_TOTAL_REGISTRATIONS: usize = MEASURED_TOTAL_REGISTRATIONS_MANAGEMENT;
#[cfg(not(feature = "management"))]
const MEASURED_TOTAL_REGISTRATIONS: usize = MEASURED_TOTAL_REGISTRATIONS_NO_MANAGEMENT;

// Both constants are compiled in both configurations on purpose: a reader
// re-freezing one can see the other, and neither can be edited by accident
// while invisible to the compiler. The `cfg` below picks which one this build
// ADJUDICATES against.

#[cfg(feature = "management")]
const BASELINE_SYNTHETIC_STUBS: usize = BASELINE_SYNTHETIC_STUBS_MANAGEMENT;
#[cfg(all(not(feature = "management"), feature = "synthetic-jdk"))]
const BASELINE_SYNTHETIC_STUBS: usize = BASELINE_SYNTHETIC_STUBS_SYNTHETIC_JDK;
#[cfg(all(not(feature = "management"), not(feature = "synthetic-jdk")))]
const BASELINE_SYNTHETIC_STUBS: usize = BASELINE_SYNTHETIC_STUBS_NO_MANAGEMENT;

/// Which registry this build measures, printed beside every number so a
/// baseline cannot be re-frozen from a run of the other configuration.
///
/// That is not hypothetical. The 1038 this file carried until 2026-08-11 was
/// six above the 1032 the same code measured — in BOTH configurations, so the
/// gap was not even feature drift. A constant documented as "the exact current
/// observed count" with "zero slack" had been quietly admitting six new stubs
/// for as long as nobody re-read the printed line beside it. An unlabelled
/// number is how that survives.
#[cfg(feature = "management")]
const MEASURED_CONFIG: &str = "management (the shipping cratonvm-cli registry)";
#[cfg(all(not(feature = "management"), feature = "synthetic-jdk"))]
const MEASURED_CONFIG: &str = "synthetic-jdk (registrars the other two arms do not compile)";
#[cfg(all(not(feature = "management"), not(feature = "synthetic-jdk")))]
const MEASURED_CONFIG: &str = "no-management (ten jmx registrars short of shipping)";

/// Name of the constant a run of THIS build should be pasted into. Emitted as
/// part of the recount line so a seed cannot land in the other configuration's
/// slot.
#[cfg(feature = "management")]
const BASELINE_CONST: &str = "BASELINE_SYNTHETIC_STUBS_MANAGEMENT";
#[cfg(all(not(feature = "management"), feature = "synthetic-jdk"))]
const BASELINE_CONST: &str = "BASELINE_SYNTHETIC_STUBS_SYNTHETIC_JDK";
#[cfg(all(not(feature = "management"), not(feature = "synthetic-jdk")))]
const BASELINE_CONST: &str = "BASELINE_SYNTHETIC_STUBS_NO_MANAGEMENT";

/// Slack added on top of the observed count when (re)freezing the baseline.
/// Documented here so the recount instructions and the constant stay in sync.
const SLACK: usize = 0;

/// THE ONE MODEL of `vm_init`'s real-JDK boot path — `VM_INIT_SEQUENCE`, the
/// replay, and the source witness over `vm/src/vm/vm_init.rs`.
///
/// It used to live here AND, near-identically, in
/// `duplicate_registration_gate.rs`. That duplication was deliberate rather
/// than overlooked — two integration-test binaries cannot share a module
/// without a file like this one — and its recorded failure mode was *redundant
/// maintenance* rather than silent disagreement, because two witnesses read the
/// same source. On 2026-08-12 the redundant maintenance came due: **both
/// witnesses located the arm with a `contains` match that hit a COMMENT quoting
/// the attribute, 44 lines above the attribute itself**, so both scanned 39
/// lines of the sibling synthetic arm, both observed 8 registrars instead of
/// 48, and both passed while asserting nothing. One locator bug, two files.
///
/// Collapsed per
/// docs/known-issues/jdk-only/W7-30-stub-ratchet-boot-path-scope.md §7. The
/// witness is compiled into both binaries and therefore runs twice per
/// `cargo test -p cratonvm-native-builtins`; that is the intended cost.
#[path = "common/vm_init_boot_path.rs"]
mod boot_path;

/// The name every census below calls. Aliased rather than re-spelled so the
/// call sites and this file's doc links are unchanged by the collapse.
use boot_path::vm_init_real_jdk_boot_path as register_boot_path;

// `register_boot_path` — the replay of `vm_init`'s real-JDK arm — now lives in
// `tests/common/vm_init_boot_path.rs` as `vm_init_real_jdk_boot_path`, shared
// with `duplicate_registration_gate.rs`. Its history (two scope fixes, both
// found by disagreement with another gate rather than by reading the replay)
// and the list of what it still cannot count are in that file's doc comments.

// SOURCE WITNESS — `the_censused_scope_is_vm_inits_boot_path` moved to
// `tests/common/vm_init_boot_path.rs` as `the_replayed_sequence_matches_vm_init`,
// alongside the model it checks. It is compiled into this binary through the
// `mod boot_path;` above, so it still runs under
// `cargo test -p cratonvm-native-builtins --test stub_ratchet`.
//
// It was BLIND when it moved, and that is the point of the move: its arm
// locator matched a COMMENT quoting `#[cfg(not(feature = "synthetic-jdk"))]` 44
// lines above the attribute, so it scanned 39 lines of the SIBLING synthetic
// arm and observed 8 registrars instead of 48 — every one of them modelled, so
// zero unmodelled, so a clean pass over a population that was not the one it
// names. The identical bug sat in `duplicate_registration_gate.rs`'s copy: one
// locator defect, two files, which is what the collapse removes.

/// Every `(class, method, descriptor, kind)` row the boot path leaves in the
/// registry, owned so the borrow of the registry can end.
///
/// `dump_registrations()` is the registry's public census API: one row per
/// surviving registration, in registration order.
fn census_rows() -> Vec<(String, String, String, NativeKind)> {
    let mut registry = NativeMethodRegistry::new();
    register_boot_path(&mut registry);
    registry
        .dump_registrations()
        .into_iter()
        .map(|(c, m, d, k)| (c.to_string(), m.to_string(), d.to_string(), k))
        .collect()
}

/// Build the default native registry the way the VM's real-JDK boot path does,
/// and return `(synthetic_stub_count, total_registrations)`.
fn census() -> (usize, usize) {
    let rows = census_rows();
    let synthetic = rows
        .iter()
        .filter(|(_, _, _, kind)| *kind == NativeKind::SyntheticStub)
        .count();
    (synthetic, rows.len())
}

/// The same census, grouped by the SOURCE FILE that registered each
/// `SyntheticStub` row.
///
/// The bare count this gate freezes says *that* the population moved; it never
/// said *where*, and the difference is the whole cost of acting on a red
/// ratchet. G83-1 recorded the gate sitting 31 over its baseline with nobody
/// able to name the 31 without writing a one-off script first; this makes the
/// breakdown fall out of the failing run itself.
///
/// Keyed by file, not by registrar function: `registered_by` carries
/// `file:line`, and the line is the `register*` call site, not the enclosing
/// `fn register_…`. Recovering the function needs per-crate source parsing (the
/// limitation `regression-suite/probes/cluster-map.py` states for the same
/// reason). File granularity is enough to answer "which subsystem moved", which
/// is what a red ratchet actually asks.
fn synthetic_by_file() -> Vec<(String, usize)> {
    let mut registry = NativeMethodRegistry::new();
    register_boot_path(&mut registry);
    let mut per_file: std::collections::BTreeMap<String, usize> = Default::default();
    for r in registry.census() {
        if r.kind != NativeKind::SyntheticStub {
            continue;
        }
        let at = r.registered_by.as_deref().unwrap_or("<unknown>");
        let file = at.replace('\\', "/");
        let file = file
            .rsplit_once(':')
            .map_or(file.as_str(), |(f, _)| f)
            .to_string();
        *per_file.entry(file).or_default() += 1;
    }
    let mut out: Vec<_> = per_file.into_iter().collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

/// THE GATE FOR STEP 3 OF
/// the retired `native-kind-is-ambient-and-defaults-to-syntheticstub` write-up:
/// *"flip the default last — once every registration states its kind,
/// `current_category` can default to something that fails loudly (or be
/// deleted)."*
///
/// It cannot be phrased as "every registration states its kind", because
/// `kind_stated` is true only for `register_with_kind` (849 of ~11,900) and is
/// deliberately false on every row a `with_category` scope covers on purpose.
/// The answerable form is the one the record actually needs: **no registration
/// may be made with no scope covering it at all**, so the conservative
/// `SyntheticStub` fallback in `NativeMethodRegistry::effective_category` is
/// dead code that can be deleted the day `NativeKind` gains a `Refuse` arm.
///
/// The count was **one** when this gate was written — `MergedAnnotation$Adapt
/// .isIn`, the first `register` in `register_essential_natives_with_shims`,
/// which ran before any `set_category` in the whole boot and therefore took the
/// constructor's default by accident rather than by decision. It is stated now.
///
/// Note what this gate is NOT. It does not say the kinds are right; the bridge
/// ratchet asks that. It does not say anybody adjudicated a row; `kind_stated`
/// and `scripts/jdk-only-kind-map.py` ask that. It says only that the *default*
/// decides nothing any more, which is the specific property step 3 names.
/// INTRINSIC RATCHET — the census cannot see this population, so a gate must.
///
/// `WORKER-3-NOTE-5` (2026-08-22) measured what the `Intrinsic` tag costs the
/// contract. `vm/src/vm/vm_exec.rs` skips it at **all three** sites that can
/// record a `native-shadows-bytecode` row:
///
/// ```text
/// if policy.is_jdk_only() && bytecode_available && kind != NativeKind::Intrinsic {
///     record_native_shadows_bytecode(class_name, method_name, descriptor, kind);
/// }
/// ```
///
/// and `resolve_step1_native` returns `DispatchDecision::Intrinsic` at step 2,
/// before the step-3 arm that records. So the census population is `Bridge` +
/// `SyntheticStub` only, by construction.
///
/// MEASURED on one `--jdk-only --dump-native-registry` run: **629** intrinsic
/// registrations, **595** owning their slot, and **398** of those standing where
/// the real JDK 25 class declares the method WITH CODE — shadows by §1.4's own
/// definition, uncountable. That is +29% on the published 1387, and 305 of the
/// 398 are `java/lang`.
///
/// **This is why the tag needs a ratchet and the header's "kept forever" needs
/// reading twice.** Re-tagging a row `Intrinsic` removes it from the census
/// *without changing behaviour*, and it would score as progress. The exemption
/// is also not uniformly benign: `String.<init>(AbstractStringBuilder,Void)V` is
/// `kind=intrinsic`, `owns_slot: true`, over real bytecode, and returned an
/// empty String for any builder on the real `byte[]` layout until
/// `WORKER-3-NOTE-6` fixed the body it reaches.
///
/// `Math`/`StrictMath` are 138 of the 398 and are what an intrinsic exemption is
/// for. The other 260 have no cited review, which is the work this gate holds
/// still while somebody does it.
///
/// Like the stub ratchet: a change that ADDS an intrinsic fails; REMOVING one is
/// welcome and only needs the baseline lowered. Raising it is allowed too — but
/// deliberately, in a commit that says which row and why, which is the whole
/// point.
#[test]
fn intrinsic_count_does_not_regress() {
    let rows = census_rows();
    let intrinsic = rows
        .iter()
        .filter(|(_, _, _, kind)| *kind == NativeKind::Intrinsic)
        .count();

    println!(
        "intrinsic-ratchet [{MEASURED_CONFIG}]: {intrinsic} Intrinsic registrations \
         out of {} total (baseline {BASELINE_INTRINSICS})",
        rows.len()
    );
    println!("intrinsic-ratchet: const BASELINE_INTRINSICS: usize = {intrinsic};");

    // WHERE it lives, on every run — a flat count says the population moved and
    // never which subsystem moved it, which is the whole cost of acting on a red
    // ratchet (the lesson `synthetic_by_file` was added for).
    for (file, n) in intrinsic_by_file() {
        println!("  intrinsic-ratchet: {n:>4}  {file}");
    }

    assert!(
        intrinsic <= BASELINE_INTRINSICS,
        "INTRINSIC RATCHET: {intrinsic} `Intrinsic` registrations, above the \
         baseline of {BASELINE_INTRINSICS}.\n\
         \n\
         `Intrinsic` is EXEMPT from the jdk-only `native-shadows-bytecode` \
         census (`vm_exec.rs`, all three recorder sites), so a row that gains \
         this tag leaves the defect population without its behaviour \
         changing. If the new rows are genuine hot-path intrinsics, raise the \
         baseline in a commit that names them and says why. If they were \
         re-tagged to quiet a census, that is the thing this gate exists to \
         stop. See `WORKER-3-NOTE-5`."
    );
}

/// The intrinsic census grouped by the SOURCE FILE that registered each row —
/// the `synthetic_by_file` shape, for the reason given there.
fn intrinsic_by_file() -> Vec<(String, usize)> {
    let mut registry = NativeMethodRegistry::new();
    register_boot_path(&mut registry);
    let mut per_file: std::collections::BTreeMap<String, usize> = Default::default();
    for r in registry.census() {
        if r.kind != NativeKind::Intrinsic {
            continue;
        }
        let at = r.registered_by.as_deref().unwrap_or("<unknown>");
        let file = at.replace('\\', "/");
        let file = file
            .rsplit_once(':')
            .map_or(file.as_str(), |(f, _)| f)
            .to_string();
        *per_file.entry(file).or_default() += 1;
    }
    let mut out: Vec<_> = per_file.into_iter().collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

/// Frozen by the run that added the gate; see `intrinsic_count_does_not_regress`.
///
/// # 1365 -> 1398, 2026-08-24 — +33, and BOTH movements are hot-path kernels
/// HotSpot intrinsifies too
///
/// The gate asks for the new rows to be NAMED, so they were measured rather
/// than argued: the per-file breakdown was taken at `2f8367356` (the commit
/// that set 1365) and at this tip, and diffed.
///
/// ```text
/// native-builtins/src/vector_support_intrinsics.rs   10 -> 38   +28
/// native-io/src/file_channel_fast_read.rs           (new) -> 5   +5
/// every other file                                      identical
///                                                                ---
///                                                                +33
/// ```
///
/// **The second column classifies it.** Totals moved 13365 -> 13398, i.e. UP by
/// EXACTLY the intrinsic delta, so every registration added in this window is
/// one of these 33 and nothing was re-tagged out of the shadow census — which is
/// the failure this gate exists for.
///
/// **+28 `VectorSupport`** (`9a117f991`). Nine entry points that HotSpot itself
/// marks `@IntrinsicCandidate` and C2 replaces with SIMD; the Java fallback an
/// interpreter runs is a lambda per operation plus a lambda per LANE, and it was
/// **93.7% of a 3500-sample profile** of GPULlama3's inference kernel. Measured
/// 3.8x (79713 -> 20813 ns/lane), one binary, kill switch
/// `CRATONVM_VECTOR_INTRINSICS=0|1` gating REGISTRATION so the off arm is
/// bit-for-bit un-intercepted, arms interleaved, checksum identical on all four
/// runs and on HotSpot. `fell_back=0` PRINTED, not claimed — and that counter
/// earned itself immediately, naming a defect (`class_id_from_mirror` not
/// resolving a primitive mirror) that a wall-clock win would have hidden.
///
/// **+5 `FileChannelImpl`**. `FileChannel.read` into a HEAP buffer measured
/// **~8.7x HotSpot** over 20 000 reads. The cause is not allocation and not the
/// temporary-direct-buffer cache — both were measured and REFUTED — it is ~20
/// JDK frames of glue per read that C2 inlines to a handful of instructions
/// around one syscall, with nothing in the profile above 11%. A distribution
/// like that only moves by removing the chain.
///
/// So both are `Intrinsic` by §1.4's own standard: a reviewed exception of the
/// kind every JVM makes for `Math.sqrt`. Neither is a row that gained the tag to
/// leave the census.
const BASELINE_INTRINSICS: usize = 1387;

#[test]
fn no_registration_runs_on_the_ambient_default() {
    let mut registry = NativeMethodRegistry::new();
    register_boot_path(&mut registry);
    let rows = registry.census();
    let unchosen: Vec<_> = rows.iter().filter(|r| !r.kind_chosen).collect();

    println!(
        "unchosen: {} of {} registrations were made with no category scope in effect",
        unchosen.len(),
        rows.len()
    );
    for r in unchosen.iter().take(40) {
        println!(
            "  {}.{}{}  kind={}  at {}",
            r.class,
            r.name,
            r.descriptor,
            r.kind.as_str(),
            r.registered_by.as_deref().unwrap_or("<unknown>")
        );
    }
    assert!(
        unchosen.is_empty(),
        "{} registration(s) inherited the registry's conservative default \
         because no `set_category` / `with_category` / `register_with_kind` \
         covered them. That is not a tag, it is the absence of one, and under \
         `--jdk-only` it is the difference between a native being registered \
         and being refused at the door. Wrap the call site in a category scope \
         or state its kind with `register_with_kind`; see the list above.",
        unchosen.len()
    );
}

/// The old, essentials-only census, kept for exactly one purpose: to assert
/// that [`census`] is a strict superset of it.
///
/// Without this, a future edit that quietly narrowed `register_boot_path` back
/// to one pass would lower the count, look like an improvement, and re-open the
/// blind spot this pair exists to close.
fn essentials_only_census() -> (usize, usize) {
    let mut registry = NativeMethodRegistry::new();
    register_essential_natives(&mut registry);
    let rows = registry.dump_registrations();
    let synthetic = rows
        .iter()
        .filter(|(_, _, _, kind)| *kind == NativeKind::SyntheticStub)
        .count();
    (synthetic, rows.len())
}

/// The census must cover strictly more than `register_essential_natives` alone.
///
/// This is the guard on the guard. The number it protects is not arbitrary: on
/// 2026-08-05 the boot registry held 364 more `SyntheticStub` rows than the
/// essentials registry, all of them in `native-collections`, and this gate was
/// blind to every one.
#[test]
fn census_covers_more_than_the_essentials_registrar() {
    let (boot_stubs, boot_total) = census();
    let (ess_stubs, ess_total) = essentials_only_census();

    println!(
        "stub-ratchet(scope): boot {boot_stubs} stubs / {boot_total} rows vs \
         essentials-only {ess_stubs} / {ess_total} \
         (delta {} stubs, {} rows)",
        boot_stubs - ess_stubs,
        boot_total - ess_total,
    );

    assert!(
        boot_total > ess_total && boot_stubs > ess_stubs,
        "the boot census ({boot_stubs} stubs / {boot_total} rows) is no larger than \
         `register_essential_natives` alone ({ess_stubs} / {ess_total}). Either a \
         registration pass was dropped from `register_boot_path`, or the passes it \
         adds have stopped registering anything — both re-open the blind spot that \
         let 364 SyntheticStub rows sit outside this gate until 2026-08-05."
    );
}

/// LIST the synthetic stubs, one `class.method descriptor` per line.
///
/// The gate above reports a NUMBER, and a number cannot be paid back: the work
/// it asks for is per-registration, so the first thing anyone who trips it
/// needs is the set. Reconstructing that set from `git blame` is worse than it
/// sounds — a line-ending normalisation commit re-blames whole files, and a
/// registration can move between registrars without changing.
///
/// Run this at the last freeze commit and at `HEAD` and diff the two outputs;
/// the difference IS the list to fix, exactly, with no attribution step.
///
/// ```text
/// cargo test -p cratonvm-native-builtins --test stub_ratchet dump_synthetic_stubs -- --nocapture
/// ```
///
/// Printing only — it asserts nothing the gate does not already assert, so it
/// cannot fail independently and cannot go stale.
#[test]
fn dump_synthetic_stubs() {
    let mut stubs: Vec<String> = census_rows()
        .into_iter()
        .filter(|(_, _, _, kind)| *kind == NativeKind::SyntheticStub)
        .map(|(class, method, descriptor, _)| format!("{class}.{method}{descriptor}"))
        .collect();
    stubs.sort();
    stubs.dedup();
    println!("@@STUBS {} distinct in [{MEASURED_CONFIG}]", stubs.len());
    for s in &stubs {
        println!("@@STUB {s}");
    }
}

/// THE GATE: the synthetic-stub count must not exceed the frozen baseline.
///
/// A failure here means a change ADDED one or more synthetic stubs to the
/// default registry. The fix is to make the new native a real `Bridge`/
/// `Intrinsic` (i.e. correct behavior) — NOT to bump the baseline. Only raise
/// the baseline when a stub is genuinely, unavoidably needed; prefer fixing the
/// underlying VM gap so real bytecode runs (see `feedback_no_synthetic_stubs`).
#[test]
fn synthetic_stub_count_does_not_regress() {
    let (synthetic, total) = census();

    // Always surface the live number (visible with `-- --nocapture`) so the
    // baseline can be (re)frozen to `synthetic + SLACK` without guessing —
    // NAMING the configuration and the constant, because the 1038 this file
    // carried until 2026-08-11 was six above the number the same code printed
    // and nobody noticed for a week.
    println!(
        "stub-ratchet [{MEASURED_CONFIG}]: {synthetic} SyntheticStub registrations \
         out of {total} total (baseline {BASELINE_SYNTHETIC_STUBS}, slack {SLACK})"
    );
    println!("stub-ratchet: const {BASELINE_CONST}: usize = {synthetic};");

    // WHERE the population lives, not just how big it is. Printed on every run,
    // pass or fail: a green ratchet whose composition shifted underneath it is
    // the case a single number is structurally unable to show.
    let by_file = synthetic_by_file();
    for (file, n) in &by_file {
        println!("stub-ratchet(by-file): {n:>5}  {file}");
    }
    // ROW-LEVEL dump, off by default (1386 lines is not gate output). Set
    // `CRATONVM_RATCHET_ROWS=1` to diff two commits' populations by NAME rather
    // than by count — the question "which stubs are the N over the baseline"
    // that a bare number cannot answer.
    if std::env::var_os("CRATONVM_RATCHET_ROWS").is_some() {
        let mut registry = NativeMethodRegistry::new();
        register_boot_path(&mut registry);
        let mut rows: Vec<String> = registry
            .census()
            .into_iter()
            .filter(|r| r.kind == NativeKind::SyntheticStub)
            .map(|r| {
                let at = r
                    .registered_by
                    .as_deref()
                    .unwrap_or("<unknown>")
                    .replace('\\', "/");
                let file = at
                    .rsplit_once(':')
                    .map_or(at.clone(), |(f, _)| f.to_string());
                format!("{}|{}{}|{}", file, r.class, r.name, r.descriptor)
            })
            .collect();
        rows.sort();
        for r in rows {
            println!("stub-ratchet(row): {r}");
        }
    }
    let breakdown = by_file
        .iter()
        .take(8)
        .map(|(f, n)| format!("{n} {f}"))
        .collect::<Vec<_>>()
        .join(", ");

    // The re-freeze target, computed once so the failure message below can be
    // written entirely with INLINE format captures and carry no positional
    // arguments at all.
    //
    // That is not a style preference. Between 2026-08-19 and 2026-08-20 this
    // `assert!` did not PARSE: merge `26e4b5db4` spliced the tail of the old
    // message onto the head of the new one and left BOTH argument lists, so the
    // first string literal ended at `stub-ratchet.md.",` and the very next token
    // was `{BASELINE_SYNTHETIC_STUBS}. A change added ...`, which is not an
    // expression. `rustfmt --check` reports `unknown start of token: \` at the
    // seam. A message with no positional arguments cannot be mis-spliced that
    // way and cannot drift out of step with its argument count.
    let refreeze = synthetic + SLACK;

    assert!(
        synthetic <= BASELINE_SYNTHETIC_STUBS,
        "STUB-RATCHET in the {MEASURED_CONFIG} configuration: {synthetic} \
         SyntheticStub natives now registered, exceeding the frozen baseline of \
         {BASELINE_SYNTHETIC_STUBS}.\n\
         \n\
         FIRST, find out WHICH rows, because this number cannot tell you why it \
         moved. Run `dump_synthetic_stubs` here and at the commit that last set \
         `{BASELINE_CONST}`, and diff the sorted `@@STUB` lines. This run already \
         printed the per-file breakdown; the top eight are: {breakdown}\n\
         \n\
         Then read each added triple, because there are TWO causes and they want \
         opposite responses:\n\
         \n\
         (a) a NEW fake was written — implement it as real bytecode, a Bridge or \
         an Intrinsic. Do NOT just raise the baseline.\n\
         \n\
         (b) an EXISTING registration changed kind, `Bridge` -> `SyntheticStub`. \
         That is a fake being labelled honestly so `--jdk-only` drops it and the \
         JDK's own bytecode runs — the opposite of a regression, and it raises \
         this count. Check `native-api/src/retired_shadow.rs` and the \
         registration site's own comment before assuming (a). On 2026-08-19, 30 \
         of 33 added rows were (b).\n\
         \n\
         CLASSIFY IT WITH THE SECOND COLUMN: total registrations are {total}, and \
         this baseline was measured beside {MEASURED_TOTAL_REGISTRATIONS}. A total \
         that did NOT move means existing fakes were relabelled (welcome; \
         re-freeze with the list). A total UP by roughly the stub delta means new \
         fakes were registered — the regression this gate exists for. A total \
         DOWN by roughly the stub delta means registrations were DELETED, which \
         is the only case where re-freezing records work rather than absorbing \
         it.\n\
         \n\
         Re-freeze `{BASELINE_CONST}` (NOT the other configuration's constant) to \
         {refreeze} only with that account written down. See stub-ratchet.md.",
    );
}

/// Sanity guard for the census itself: if `register_essential_natives` ever
/// stops registering anything (a wiring break), the ratchet would pass
/// vacuously. Assert the registry is non-trivially populated so a "0 stubs
/// because 0 registrations" never masquerades as a green ratchet.
#[test]
fn essential_registry_is_populated() {
    let (_synthetic, total) = census();
    // `> 100` was too weak to be a vacuity guard. The ratchet is a ratio
    // argument — "157 of 9,320" — and the denominator was never asserted, so
    // a wiring break that dropped 9,000 registrations would still leave
    // `total` above 100 and the ratchet green with far fewer stubs. This floor
    // sits well below the live count (9,320 as of 2026-07-30) so ordinary
    // churn does not trip it, but a collapse of the surface does.
    // Raised 8,000 -> 11,000 on 2026-08-05 with the census scope. Against the
    // 11,649-row boot registry the old floor would not have noticed losing the
    // whole of `register_collections_natives` (2,279 rows) — the exact failure
    // this test exists to detect. Keep it within ~5% of the live total.
    //
    // Raised 11,000 -> 11,800 on 2026-08-11 with the second scope fix: the live
    // total is 12,445 (no-management) / 12,758 (management), so the old floor
    // had drifted back to 12% of headroom and would again have slept through
    // the loss of a 1,000-row registrar. The floor is set from the SMALLER
    // configuration on purpose — a floor that only holds in the build with more
    // registrars in it is not a floor.
    const MIN_TOTAL_REGISTRATIONS: usize = 11_800;
    assert!(
        total >= MIN_TOTAL_REGISTRATIONS,
        "register_essential_natives produced only {total} registrations, below the \
         {MIN_TOTAL_REGISTRATIONS} floor — the census entrypoint or a whole \
         registration module looks broken, which would make the stub-ratchet pass \
         vacuously. Expected the real-JDK boot path to register ~9,300 natives."
    );
}

// ---------------------------------------------------------------------------
// STRICT (JDK-only) CENSUS — docs/feature-designs/jdk-only-mode.md §4 and §11
//
// Strictness is a *runtime policy* on the registry, not a build feature: the VM
// calls `set_compatibility_mode(CompatibilityMode::JdkOnly)` once at init,
// BEFORE any `register_*` pass (contract §8), and `register()` then refuses a
// `NativeKind::SyntheticStub` outright, recording a
// `JdkOnlyViolation::SyntheticNativeRegistered` so the run can name what went
// missing.
//
// The three tests below measure that same `register_essential_natives` surface
// through the strict policy. They are purely additive: the compatibility-mode
// baseline above is untouched, and nothing here tightens
// `BASELINE_SYNTHETIC_STUBS`. Wave 1 is measurement, not deletion (contract
// §10) — the point of these tests is to make the size and shape of the backlog
// a number CI prints on every run, not to delete anything yet.
//
// The same zero-stub invariant is asserted against a hand-built synthetic mix
// in `native-api/tests/jdk_only_registry.rs`; here it is asserted against the
// real boot-path registrar, which is the one that has 549 stubs in it.
// ---------------------------------------------------------------------------

/// Vacuity floor for the *strict* registry, mirroring `MIN_TOTAL_REGISTRATIONS`
/// in [`essential_registry_is_populated`].
///
/// This is a collapse detector, not a measurement. Strict mode is expected to
/// shed the stub registrations plus whatever aliases hang off them (see
/// [`strict_registry_drops_only_the_stubs`] for why that fallout is real), so
/// the floor sits well below the compatibility-mode floor. If the strict
/// registry ever drops under it, `set_compatibility_mode` is refusing far more
/// than the stubs, and "zero synthetic stubs" would be true only because the
/// registry is empty.
///
/// # 10,500 -> 10,200, 2026-08-10
///
/// Lowered by 300 for a strict registry of 10,449, and the number it is
/// tracking moved for two independent reasons on the same day: 179
/// registrations were deleted with `ensure_synthetic_class`, and 248 were
/// re-tagged `SyntheticStub` because no supported JDK image declares their
/// receiver class (`native-api/src/no_image_receiver.rs`). Strict mode refuses
/// the second group by design — that is the re-tag's whole point.
///
/// **Lowering a collapse detector is exactly the move it exists to make
/// suspicious, so it is justified by the evidence the detector cannot see.**
/// Against a binary built from `dev` without the re-tag, on the same host and
/// the same images: the `--jdk-only` corpus is 52 passed / 6 failed on **both**
/// arms and compatible mode is 35 / 0 on both, and the `CRATONVM_NO_STUBS` drop
/// list grows by exactly 248 entries with **zero** entries moving the other way.
/// A registry shedding whole modules does not produce that diff.
///
/// The 300 of headroom is deliberate and is not a prediction: it keeps the
/// detector a detector after the next re-tag of this size. The 2026-08-11
/// retirement of `java.util.logging`'s 104 shadow rows is one such re-tag and
/// moved this total by zero: a re-tag changes a registration's KIND, it does
/// not remove the registration. Record: the retired
/// `bridge-reclassification-wave` write-up.
///
/// # 10,200 -> 10,900, 2026-08-11
///
/// Raised, not lowered, and for a reason that is not a measurement at all: the
/// census now replays 46 of `vm_init`'s registrars instead of 6, so the strict
/// registry it observes went 10,439 -> 11,192 (no-management) / 11,495
/// (management). Nothing about strict mode changed. As with
/// `MIN_TOTAL_REGISTRATIONS`, the floor takes the SMALLER configuration and
/// keeps the same ~300 rows of deliberate headroom, so it survives the next
/// re-tag of that size while still detecting a shed module.
const STRICT_MIN_TOTAL_REGISTRATIONS: usize = 10_900;

/// Build the default native registry the way `--jdk-only` does: set the
/// VM-scoped strict policy *first*, then run the same boot sequence
/// `vm/src/vm/vm_init.rs` runs (see [`register_boot_path`]).
///
/// Ordering is load-bearing and mirrors contract §8: a mode set *after*
/// registration would leave every stub already in the table and make this whole
/// section pass for the wrong reason.
///
/// Returns `(synthetic_stub_count, total_registrations, refused_triples)`. The
/// refusals are returned as triples, not a count: a triple registered by two
/// passes is refused twice, so the count alone cannot be compared against
/// surviving rows — see `strict_registry_drops_only_the_stubs`.
fn strict_census() -> (usize, usize, Vec<(String, String, String)>) {
    let mut registry = NativeMethodRegistry::new();
    registry.set_compatibility_mode(CompatibilityMode::JdkOnly);
    // The same full boot sequence the compatibility census runs (see
    // `register_boot_path`), not `register_essential_natives` alone — otherwise
    // "strict mode refuses N registrations" is a count over a fraction of the
    // registry, and the number this prints is not the number an operator sees
    // in `--jdk-only-report`.
    register_boot_path(&mut registry);

    let rows = registry.dump_registrations();
    let synthetic = rows
        .iter()
        .filter(|(_, _, _, kind)| *kind == NativeKind::SyntheticStub)
        .count();
    let total = rows.len();
    drop(rows);
    let refused = registry
        .refused_registrations()
        .iter()
        .filter_map(|v| match v {
            cratonvm_types::error::JdkOnlyViolation::SyntheticNativeRegistered {
                class,
                method,
                descriptor,
                ..
            } => Some((class.clone(), method.clone(), descriptor.clone())),
            _ => None,
        })
        .collect();
    (synthetic, total, refused)
}

/// Every registration the strict boot makes, as `census_rows` does for the
/// compatible one.
fn strict_rows() -> Vec<(String, String, String, NativeKind)> {
    let mut registry = NativeMethodRegistry::new();
    registry.set_compatibility_mode(CompatibilityMode::JdkOnly);
    register_boot_path(&mut registry);
    registry
        .dump_registrations()
        .into_iter()
        .map(|(c, m, d, k)| (c.to_string(), m.to_string(), d.to_string(), k))
        .collect()
}

/// **A fake must not outlive `--jdk-only` because a SECOND file called it a
/// bridge.** Contract §11's "zero synthetic-stub invocations through any path",
/// asserted per triple instead of per row count.
///
/// `strict_registry_drops_only_the_stubs` bounds row *counts*, and its own
/// comment names the reason that cannot express this: "a triple can be
/// registered with two different kinds". When it is, the drop happening **at
/// registration** decides the outcome — strict mode refuses the `SyntheticStub`
/// row at the door, so the `Bridge` row registered by the other file survives to
/// own the slot, and the method the mode exists to remove goes on being served
/// by the fake. Compatible mode, where nothing is refused, keeps the stub
/// instead: **the two modes run different implementations of the same method**,
/// and which is which is a property of the order two files happen to run in.
///
/// Measured on JDK 25 / linux, 2026-08-06, by diffing a `--jdk-only` census
/// against a `--real-jdk` one: 58 triples, in two families —
/// `StampedLock`/`ServiceLoader`/`URLCodec` (the registrar had no category at
/// all, so its callers disagreed) and `Function$Identity.apply` /
/// `UnaryOperator.identity` / `CountDownLatch` / `CyclicBarrier` /
/// `InputStream.transferTo` (a second file under a `Bridge` scope). The first
/// two of those are the successor defect the retired
/// `native-kind-is-ambient-and-defaults-to-syntheticstub` write-up names: a
/// surviving `Bridge` whose receiver class contract §5 forbids fabricating.
///
/// `Intrinsic` is not flagged. A triple that is a stub in compatible mode and an
/// intrinsic in strict is a *different implementation*, not a surviving fake —
/// `java.util.Set.of` is the whole of that set today — and an intrinsic
/// shadowing concrete bytecode is what an intrinsic is.
#[test]
fn no_fake_survives_strict_mode_as_someone_elses_bridge() {
    let mut compat: std::collections::BTreeMap<(String, String, String), NativeKind> =
        std::collections::BTreeMap::new();
    for (c, m, d, k) in census_rows() {
        // Last write wins, exactly as the registry's slot does.
        compat.insert((c, m, d), k);
    }

    let mut survivors: Vec<String> = Vec::new();
    for (c, m, d, k) in strict_rows() {
        if k != NativeKind::Bridge {
            continue;
        }
        if compat.get(&(c.clone(), m.clone(), d.clone())) == Some(&NativeKind::SyntheticStub) {
            survivors.push(format!("{c}.{m}{d}"));
        }
    }
    survivors.sort();
    survivors.dedup();

    println!(
        "stub-ratchet(strict): {} triple(s) are a SyntheticStub in compatible mode \
         and a Bridge in strict",
        survivors.len()
    );
    assert!(
        survivors.is_empty(),
        "{} triple(s) are tagged `SyntheticStub` where compatible mode dispatches \
         them and `Bridge` where strict mode does, so `--jdk-only` runs the fake \
         the mode exists to remove — and it does so only because the stub \
         registration is refused at the door, leaving the other file's `Bridge` \
         row to own the slot. Decide the kind ONCE, at both sites:\n  {}",
        survivors.len(),
        survivors.join("\n  ")
    );
}

/// Acceptance criterion (contract §11): **the final native registry in strict
/// mode contains zero `SyntheticStub` entries.**
///
/// This passes *today*, and it is worth being precise about why: not because
/// the 549 stubs are gone, but because `register()` refuses them at the door
/// under `JdkOnly`. That is exactly the property CI's zero-stub census asserts
/// against a booted VM, so it is worth pinning here too — it is the cheap,
/// hermetic version of the same check, with no JDK image and no subprocess.
///
/// A failure means a `SyntheticStub` reached the live table despite the strict
/// policy: either a registrar bypasses `register()`, or the mode is being set
/// after registration rather than before it.
#[test]
fn strict_registry_has_zero_synthetic_stubs() {
    let (strict_stubs, strict_total, refused) = strict_census();

    println!(
        "stub-ratchet(strict): {strict_stubs} SyntheticStub registrations out of \
         {strict_total} total; {} registrations refused by JdkOnly",
        refused.len()
    );

    assert_eq!(
        strict_stubs, 0,
        "acceptance criterion (contract §11) violated: {strict_stubs} SyntheticStub \
         natives are in the STRICT registry. Under CompatibilityMode::JdkOnly, \
         register() must refuse every SyntheticStub, so a non-zero count means a \
         registrar reached the slot table without going through register(), or \
         set_compatibility_mode was applied after registration instead of before it."
    );

    assert!(
        strict_total >= STRICT_MIN_TOTAL_REGISTRATIONS,
        "the strict registry holds only {strict_total} registrations, below the \
         {STRICT_MIN_TOTAL_REGISTRATIONS} floor. Zero synthetic stubs is then a \
         statement about an empty registry, not about the stub backlog."
    );
}

/// Strict mode drops the stubs — and, modulo aliasing, *only* the stubs.
///
/// The tempting assertion is exact subtraction:
/// `strict_total == compat_total - compat_stubs`. It is wrong, in a way worth
/// writing down because it will look like a bug to the next reader.
///
/// `NativeMethodRegistry::alias_class` (used by the JDBC, JBoss-MSC and
/// `net_phase_e` registrars) copies a class's natives to a second name by
/// walking the **live registration log** and re-`register()`ing each row it
/// finds. Under `JdkOnly` a refused stub never enters that log, so the alias
/// pass finds nothing to copy and the alias disappears too — one refusal can
/// remove more than one row. Hence:
///
/// * `compat_total - strict_total >= compat_stubs` — every stub is gone, plus
///   any aliases derived from one.
/// * `refused <= compat_stubs` — the mirror image. An alias copy that was
///   itself stub-tagged in compatible mode is counted in `compat_stubs`, but in
///   strict mode `alias_class` never attempts it, so no refusal is recorded for
///   it. `refused` counts refusals, not stubs.
///
/// Both bounds are one-sided on purpose. Pinning the alias fallout to an exact
/// number would make this test fail every time a registrar adds or removes an
/// `alias_class` call, which is churn, not regression.
#[test]
fn strict_registry_drops_only_the_stubs() {
    let compat_rows = census_rows();
    let compat_total = compat_rows.len();
    let compat_stubs = compat_rows
        .iter()
        .filter(|(_, _, _, kind)| *kind == NativeKind::SyntheticStub)
        .count();
    let (_strict_stubs, strict_total, refused_triples) = strict_census();

    let dropped = compat_total.saturating_sub(strict_total);

    println!(
        "stub-ratchet(strict): compatible {compat_total} rows ({compat_stubs} stubs) \
         -> strict {strict_total} rows; {dropped} rows dropped, {} refusals recorded",
        refused_triples.len()
    );

    assert!(
        strict_total <= compat_total,
        "strict mode registered MORE than compatible mode ({strict_total} > \
         {compat_total}). JdkOnly only ever refuses; it must never add a registration."
    );

    assert!(
        dropped >= compat_stubs,
        "strict mode dropped only {dropped} registrations but compatible mode has \
         {compat_stubs} SyntheticStub rows. Every stub row must be absent from the \
         strict registry (plus possibly some alias_class fallout, which is why this \
         is a lower bound and not equality). A shortfall means some stubs survive \
         the strict policy."
    );

    // The property is CONTAINMENT, not a count comparison. `refused <=
    // compat_stubs` was the old form and it is wrong once the census runs the
    // real boot sequence: a triple registered by two passes — which the boot
    // path does deliberately, `register_forkjoin_quiescence` overriding
    // `register_concurrent_natives` by last-write-wins — yields ONE surviving
    // row in compatible mode and TWO refusals in strict. On 2026-08-05 that read
    // 598 refusals against 549 stub rows and failed a gate that was measuring
    // nothing wrong.
    //
    // THE THIRD BOUND WAS DELETED ON 2026-08-05, DELIBERATELY. Do not re-add it
    // without reading this.
    //
    // It asserted `refused <= compat_stubs`, justified as "a refusal with no
    // corresponding stub means JdkOnly is rejecting a Bridge or an Intrinsic".
    // The property is real — contract §4 — but it is **not observable from a
    // differential census**, and the boot-path scope fix made that impossible to
    // ignore. Three independent reasons, each measured:
    //
    //  1. **Refusal is per-ATTEMPT; the registry is per-TRIPLE.** A triple
    //     registered by two passes yields ONE surviving compat row and TWO
    //     refusals, so the counts are not comparable at all — 598 refusals
    //     against 549 stub rows here, with nothing wrong.
    //  2. **Compatible mode has drop rules of its own, and they run AFTER the
    //     JdkOnly refusal in `register()`.** `drop_real_layout_synthetic` (which
    //     `vm_init` sets, and which this census now mirrors) drops every
    //     `java/util/EnumSet` native outright, so strict *records* a refusal for
    //     `EnumSet.noneOf` while compat holds no row for it — neither surviving
    //     nor overwritten. Both behaviours are correct.
    //  3. **A triple can be registered with two different kinds.**
    //     `java/nio/ByteBuffer.allocate(I)` is registered `SyntheticStub` by one
    //     pass and non-stub by another, so it is simultaneously refused (the
    //     stub attempt) and present in the strict registry (the other one).
    //     Even "a refused triple is absent from the strict registry" is false.
    //
    // Reasons 2 and 3 are not fixable by a cleverer query: the registry does not
    // retain the kind of every attempt, only of survivors plus one level of
    // `overwrote`. A gate that cannot mean what it claims is decoration, and
    // this feature has shipped three of those already — so it goes, rather than
    // being weakened until it passes.
    //
    // The property is enforced where it belongs: structurally in `register()`,
    // which refuses on `!current_category.allowed_in(JdkOnly)` and so can refuse
    // no other kind by construction, and hermetically in
    // `native-api/tests/jdk_only_registry.rs`, which asserts it against a
    // hand-built kind mix where no drop rule can confound the answer.
    //
    // The two bounds above survive because both are counts over survivors on
    // both sides, which duplicate registration and compat-side drops cannot
    // skew in the direction they assert.

    assert!(
        strict_total >= STRICT_MIN_TOTAL_REGISTRATIONS,
        "the strict registry holds only {strict_total} registrations, below the \
         {STRICT_MIN_TOTAL_REGISTRATIONS} floor — strict mode is shedding whole \
         registration modules, not just stubs."
    );
}

/// THE END-STATE GATE, deliberately `#[ignore]`d.
///
/// [`strict_registry_has_zero_synthetic_stubs`] passes today for a weak reason:
/// `register()` refuses the stubs at the door. The 549 registrations still
/// exist in `native-builtins/src/`, still run on every boot, and are still what
/// an ordinary `--real-jdk` run dispatches into. **Refused is not retired.**
///
/// This test asserts the strong property — strict mode has *nothing to refuse*
/// — and stays ignored until all three of the following have landed:
///
/// 1. **Reclassify or delete the 549 `SyntheticStub` registrations** in
///    `native-builtins/src/`, subsystem by subsystem: each one becomes a real
///    `Bridge`/`Intrinsic` because it genuinely crosses a VM boundary, or it
///    goes away so the real JDK bytecode runs. This is explicitly *not* wave 1
///    work (contract §8: "do not edit `native-builtins/src/lib.rs`; the
///    549-stub reclassification is a separate wave with its own
///    subsystem-per-PR discipline").
/// 2. **Drive [`BASELINE_SYNTHETIC_STUBS`] to 0 in the same change** that
///    removes the last one. The ratchet is slack-free by design; leaving the
///    baseline at 549 after the stubs are gone would silently re-admit 549 new
///    ones.
/// 3. **Un-ignore this test** (delete the `#[ignore]`) so the zero is held,
///    and promote the CI `jdk-only` job from advisory to blocking, which is the
///    posture contract §11 calls for.
///
/// Until then it is run on demand:
/// `cargo test -p cratonvm-native-builtins --test stub_ratchet -- --ignored --nocapture`
#[test]
#[ignore = "wave 1 is measurement: the 549 stubs are refused at registration, not yet retired"]
fn strict_mode_refuses_nothing() {
    let (_strict_stubs, _strict_total, refused) = strict_census();
    let refused = refused.len();

    assert_eq!(
        refused, 0,
        "{refused} SyntheticStub registrations still have to be refused at VM init. \
         Zero refusals is the real end state: it means the stubs were reclassified \
         or deleted at the source, not merely filtered out of the table on the way \
         in. See this test's doc comment for the three steps that must land first."
    );
}

/// F24-1 found it, F33-1 fixed it (both 2026-08-13) — **no `SharedSecrets`
/// factory may outlive the owner `--jdk-only` drops out from under it.**
///
/// `register_wp1_4_shared_secrets` sets one ambient kind, `Bridge`, for the
/// whole registrar (`shared_secrets_bridge.rs`). `NativeMethodRegistry::register`
/// then re-tags by RECEIVER CLASS, via
/// `no_image_receiver::receiver_declared_by_no_supported_image` — and the
/// receiver of a *factory* registration is `SharedSecrets`, not the object the
/// factory hands out. Those two facts split one registrar down the middle:
///
///  * the factories go on `jdk/internal/access/SharedSecrets`, a real JDK class
///    on no table, so they stayed `Bridge` and **survived `--jdk-only`**;
///  * the four `cratonvm/internal/ss/…$1` owners are in
///    `VM_MINTED_STAND_IN_RECEIVERS`, so every method on them is re-tagged
///    `SyntheticStub` and **is dropped in `--jdk-only`**.
///
/// So strict mode kept four natives shadowing real JDK bytecode getters that
/// handed back a carrier with no implementation on it — and silently, because
/// `alloc_singleton`'s `Err` arm returns a `ClassId(0)` object rather than
/// failing. `register_factories` now derives the factory's kind from the OWNER,
/// so the two halves refuse or survive together.
///
/// # What this test pins, and why it is not the unit test
///
/// `factory_kind_follows_the_owner_it_hands_out` (in `shared_secrets_bridge.rs`)
/// checks the same rule against a hand-built registry. This one checks it
/// against the **boot registry in `CompatibilityMode::JdkOnly`** — the table an
/// operator's `--jdk-only` run actually holds — so it also covers the ways a row
/// can come back that a unit test cannot see: a second registrar re-registering
/// the same triple under a `Bridge` scope (the shape
/// `no_fake_survives_strict_mode_as_someone_elses_bridge` exists for), or a
/// `set_compatibility_mode` ordering change. The rule is stated in two places on
/// purpose, at two scopes; that is not duplication.
///
/// The premise is asserted rather than assumed, because this ratchet has had a
/// scope hole twice: `register_wp1_4_shared_secrets` reaches
/// [`register_boot_path`] only *transitively*, through
/// `register_essential_natives_with_shims` (`native-builtins/src/lib.rs:10063`).
/// A grep of the boot-path replay for `shared_secrets` finds nothing, so the day
/// that indirection changes, every assertion below would pass on an empty set.
///
/// **What replaced what, and why the old shape had to go.** F24-1 froze the
/// ORPHAN SET — the owners strict mode leaves with no registered method — at
/// these four, so that growth reddened it and a fix reddened it too. That was
/// right for a defect nobody could yet fix, and it is wrong now for a reason
/// worth writing down: **the fix does not change the orphan set.** Refusing the
/// four factories leaves those four owners exactly as method-less in strict mode
/// as they were, so the old assertion stays GREEN across the repair and pins
/// nothing about it. What actually changed is the PAIRING, so the pairing is
/// what this asserts.
#[test]
fn no_shared_secrets_factory_outlives_the_owner_strict_mode_drops() {
    use std::collections::BTreeSet;

    let rows = strict_rows();
    let live_classes: BTreeSet<&str> = rows.iter().map(|(c, _, _, _)| c.as_str()).collect();

    assert!(
        live_classes.contains("jdk/internal/access/SharedSecrets"),
        "the SharedSecrets factories are not in this ratchet's scope any more, so \
         this test — and the SyntheticStub count — just went blind to ~250 \
         registrations. Re-check `register_essential_natives_with_shims`."
    );

    let owners: Vec<&'static str> =
        cratonvm_native_builtins::shared_secrets_bridge::owner_classes().collect();
    assert!(
        !owners.is_empty(),
        "owner_classes() is empty; the projection this test reads is gone"
    );

    // Which factory methods `--jdk-only` still serves, whatever their kind.
    let live_factories: BTreeSet<&str> = rows
        .iter()
        .filter(|(c, _, _, _)| c.as_str() == "jdk/internal/access/SharedSecrets")
        .map(|(_, m, _, _)| m.as_str())
        .collect();

    let mut mismatched: Vec<String> = Vec::new();
    let mut refused_factories: Vec<&'static str> = Vec::new();
    for (method, owner) in
        cratonvm_native_builtins::shared_secrets_bridge::factory_methods_and_owners()
    {
        let factory_lives = live_factories.contains(method);
        let owner_lives = live_classes.contains(owner);
        if factory_lives != owner_lives {
            mismatched.push(format!(
                "SharedSecrets.{method}() {} but its owner `{owner}` {}",
                if factory_lives {
                    "SURVIVES"
                } else {
                    "is refused"
                },
                if owner_lives {
                    "keeps its methods"
                } else {
                    "has every method dropped"
                },
            ));
        }
        if !factory_lives {
            refused_factories.push(method);
        }
    }

    assert!(
        mismatched.is_empty(),
        "{} SharedSecrets factory/owner pair(s) disagree about `--jdk-only`. A \
         surviving factory over a dropped owner shadows the real JDK getter and \
         returns a carrier with no methods — `alloc_singleton`'s `Err` arm makes \
         that a WRONG-CLASS RECEIVER, not an error, so nothing reports it. A \
         refused factory over a live owner removes a working bridge for nothing. \
         Fix the kind derivation in `register_factories`; do not relax this. See \
         docs/known-issues/jdk-only/F33-1-a-factory-and-its-owner-must-share-one-kind-20260813.md\n  {}",
        mismatched.len(),
        mismatched.join("\n  "),
    );

    refused_factories.sort_unstable();
    assert_eq!(
        refused_factories,
        [
            "getJavaIORandomAccessFileAccess",
            "getJavaNetHttpCookieAccess",
            "getJavaNetUriAccess",
            "javaUtilJarAccess",
        ],
        "the set of SharedSecrets accessors `--jdk-only` refuses has changed. \
         GROWTH means a new fabricated owner, or a real owner newly added to \
         `NO_IMAGE_JDK_RECEIVERS`; the assertion above already forced its factory \
         to follow, so this line is where you say you meant it. SHRINKAGE means a \
         carrier was retargeted onto the JDK's own implementation class — a real \
         fix — and this list shrinks with it. Note `javaUtilJarAccess` has no \
         `get` prefix; that is the JDK's spelling (F24-1), not a typo."
    );
}
