// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Receiver classes that **no supported JDK image declares**, and why a
//! `Bridge` registered on one of them is a contradiction rather than a bridge.
//!
//! # The rule
//!
//! `docs/feature-designs/jdk-only-mode.md` §1.5 defines a `Bridge` as what an
//! `ACC_NATIVE` method binds to. If no image on any supported (version,
//! platform) pair declares the receiver *class*, there is no `ACC_NATIVE`
//! method for the registration to bind to and there never can be — so the row
//! is not a bridge under any reading. It is CratonVM's own implementation of a
//! shape CratonVM alone mints, which is precisely what
//! [`NativeKind::SyntheticStub`](crate::registry::NativeKind::SyntheticStub)
//! names.
//!
//! The rule needs no census of *what the VM mints*, and that matters: no such
//! census exists or can easily be built, because the mint sites pass the class
//! name as a `const`, a local, or a loop variable and a grep over them
//! under-reports. Both branches of the disjunction give the same answer:
//!
//! * the VM mints the receiver → the native is that stand-in's implementation,
//!   i.e. a synthetic stub; or
//! * nothing can ever produce a receiver → the tag decides nothing and
//!   `SyntheticStub` is inert.
//!
//! # Why this is applied centrally and not at the 165 registration sites
//!
//! The property is a fact about the *class*, measured against six images. A
//! registration site cannot know it, and 165 sites restating it would drift the
//! first time an image changed. `NativeMethodRegistry::register` already
//! carries class-scoped policy of exactly this shape (the `java/lang/String`
//! bridge drop, the `CRATONVM_REAL_NET_SOCKETS` socket drop), and the
//! per-registration kind stays reviewable because
//! `scripts/jdk-only-kind-map.py` freezes every row's kind.
//!
//! A registration re-tagged here reports `kind_stated` — the kind *was*
//! adjudicated, by measurement rather than by an author — so it leaves the
//! "inherited an ambient `set_category`" population honestly.
//!
//! # What is measured, and what would invalidate it
//!
//! Measured 2026-08-10 against **six** images — Temurin 21.0.12+8 and
//! 25.0.4+7, each of linux/x64, windows/x64 and macos/x64 — with one CratonVM
//! binary and one workload, only `--java-home` differing. A class is listed
//! only when `image_declaring_method.image_has_class` is `false` on **all
//! six**. `docs/jdk-only-migration.md` puts the supported matrix at JDK 21 and
//! 25 on Linux and Windows; macOS is swept as well because
//! `sun/nio/ch/KQueuePort` is on no linux or windows image and is on both macOS
//! ones — a two-platform sweep called it dead.
//!
//! `scripts/jdk-only-no-image-receivers.py` re-derives this table from a set of
//! censuses and fails on any drift. Run it whenever the supported image matrix
//! changes; a name that gains a declaration must leave this list, because
//! keeping it would demote a real bridge to a stub, which is the 2026-07-14
//! `java.util.Properties` regression shape.
//!
//! # What is deliberately NOT here
//!
//! * **Third-party names** (`org/springframework/…`, `io/netty/…`). Absent
//!   from a JDK image by construction; the application supplies the class, and
//!   a native there shadows application bytecode — a different question with a
//!   different answer.
//! * **Reviewed VM services.** §11 admits "a reviewed VM service" to strict
//!   mode, and `NativeKind` has no variant that says so, so those keep `Bridge`
//!   and are enumerated in [`VM_SERVICE_RECEIVERS`] with the reason. Tagging
//!   them here would take `-javaagent`, dynamic proxies, the TLS self-test,
//!   the JDBC SPI probe and `LinkedList.listIterator()` out of `--jdk-only`.
//!
//!   The last of those is the 2026-08-11 correction, and it is the shape to
//!   watch for: a `cratonvm/…` name is *not* self-evidently a stand-in. The
//!   test is whether it stands in for a JDK shape a caller could otherwise
//!   hold, and `LinkedListSnapshotListItr` was named out of the `java/util/*`
//!   namespace precisely so it would NOT inherit `LinkedList$ListItr`'s
//!   layout. It landed on the stand-in list by prefix, not by that test.
//! * **Receivers strict mode still creates**, in [`STRICT_STILL_FABRICATES`].
//!   Their natives are not bridges either; they cannot be re-tagged yet for a
//!   reason that is not about them.
//!
//! # The other half, and the two ways it was measured wrong
//!
//! Re-tagging is only half a fix, and the L5 record said so before this table
//! existed: a `Bridge` whose receiver class §5 forbids is held together by the
//! fabrication path recording the violation and minting the class anyway. Drop
//! the natives without closing that, and strict mode mints the object and then
//! cannot dispatch on it — `UnsatisfiedLinkError` where it used to work.
//!
//! **First measurement.** A `--dump-class-origins` census of three strict runs
//! said **five** of the 56 classes here were still created under `--jdk-only`:
//! the two proxy names (`vm-internal`, reviewed VM services) and
//! `HashMap$KeyItr` plus the three `Atomic*FieldUpdater$RustJvmImpl`
//! (`compatibility-stub`, §5's violation by name). Those four were held back.
//! Worth recording how they were found, because the cheaper instrument missed
//! most of them: the strict corpus caught **two** (`RJdkProxy`,
//! `RChmKeySetView`) and the other three were latent — no vector builds an
//! atomic field updater.
//!
//! **Then the blocker went away.** `ClassManager::ensure_synthetic_class` was
//! deleted the same day
//! (`jdk-only-ensure-synthetic-class-deleted-FIXED-20260810.md`), and
//! with the infallible funnel gone strict mode creates none of the four: a
//! `--jdk-only` run of `RChmKeySetView` dies on
//! `NoClassDefFoundError: java/util/HashMap$KeyItr`, which is §5 enforcing
//! instead of recording. All four moved into the re-tag and
//! [`STRICT_STILL_FABRICATES`] is empty.
//!
//! One wrong turn on the way, kept because the correction is the useful part:
//! `RChmKeySetView` reddening was first read as this table's doing, and
//! `HashMap$KeyItr` was put back on the strength of it. That vector reddens on
//! unmodified `dev` too, for the refusal above. **Attribute a corpus failure by
//! running the same vector on a binary without the change** — that one step is
//! what separated the two readings, and neither the census nor the corpus could
//! do it alone:
//!
//! ```text
//! cratonvm --jdk-only --java-home <JDK> --dump-class-origins cls.json \
//!     -cp probes DeadSweepReachProbe
//! CRATONVM_ARGS=--jdk-only SUITE=all bash regression-suite/run.sh
//! # then re-run any red vector on a build WITHOUT this table
//! ```

/// JDK-namespaced receiver classes declared by none of the six swept images.
///
/// Sorted and unique; [`table_is_sorted_and_unique`] enforces both.
///
/// Two entries already carried `SyntheticStub` before this table existed —
/// `java/util/Comparator$Native` and `java/util/function/Function$Identity`.
/// They are kept here rather than left implicit: they are the precedent this
/// table generalises, and a list that silently omitted the two rows everybody
/// agrees about would be harder to check, not easier.
///
/// # F24-1 (2026-08-13): re-measured on one image, and one candidate NOT added
///
/// All 49 entries were re-checked with `javap -p <name>` on Microsoft
/// **25.0.3+9-LTS**, windows/x64: **zero** are declared. That is one of the six
/// images, so it can only corroborate, never complete, the claim — but it is
/// the direction that matters, because a *positive* hit is what demotes a real
/// bridge (the 2026-07-14 `java.util.Properties` shape) and there were none.
/// The sweep was run with positive controls (`sun.management.VMManagementImpl`,
/// `com.sun.jmx.mbeanserver.MXBeanMapping`, `jdk.internal.ref.CleanerFactory`,
/// `sun.nio.ch.FileDispatcherImpl`, …) so that "javap cannot see the module" is
/// distinguishable from "the class is absent" — without them a sweep of this
/// shape reports its own reach.
///
/// **`java/security/AccessController$1` STAYS, and as of 2026-08-13 it is
/// INERT — which is the end state, not a leftover.** The history is worth one
/// paragraph because the entry looks deletable and is not:
///
///  * F17-1 deleted the `getJavaSecurityAccess` factory (JEP 486 removed
///    `jdk.internal.access.JavaSecurityAccess` outright) but kept
///    `register_java_security_access`'s two registrations on this class. This
///    entry was then **the only thing tagging them `SyntheticStub`**, and
///    deleting the line would have promoted two fabricated natives to `Bridge`
///    and admitted them to `--jdk-only` — the exact inversion of the intent.
///  * F33-1 deleted those registrations (`native-builtins/src/shared_secrets_bridge.rs`,
///    see the `// JavaSecurityAccess — DELETED` note there). **Only then** did
///    this row become inert, and the ORDER was the whole difficulty: the row had
///    to outlive the registrations, not the other way round.
///
/// It is kept because the image fact is TRUE and independently re-measured —
/// `javap -p 'java.security.AccessController$1'` → `class not found` on
/// Microsoft 25.0.3+9-LTS — so `scripts/jdk-only-no-image-receivers.py` should
/// go on checking it. An entry whose registrations are gone costs one `&str` in
/// a binary search and buys the next author the same protection F17-1's
/// successor needed.
///
/// **`java/io/ObjectInputStream$1` is a candidate and is deliberately NOT added
/// here.** It meets the rule on the one image measured — absent on 25.0.3, and
/// `jdk25src/java.base/java/io/ObjectInputStream.java:4039` shows why
/// (`SharedSecrets.setJavaObjectInputStreamAccess(ObjectInputStream::checkArray)`
/// is a method reference, so the access object is an `invokedynamic` call site
/// and there is no anonymous class) — and `register_java_object_input_stream_access`
/// registers on it today, `Bridge`, surviving strict. But this table's threshold
/// is **all six** images and no JDK 21 image or source is available on this
/// host, so a one-image add would be the under-measurement the module docs warn
/// about, in the direction that breaks things. Run
/// `scripts/jdk-only-no-image-receivers.py` when a 21 image is at hand. The rows
/// are inert meanwhile: the bridge registers no factory for this owner, and no
/// bytecode can produce a receiver of a class that does not exist.
pub const NO_IMAGE_JDK_RECEIVERS: &[&str] = &[
    "com/sun/jmx/mbeanserver/MappedMXBeanType",
    "com/sun/jmx/mbeanserver/OpenConverter",
    "com/sun/net/httpserver/HttpExchange$ResponseBody",
    "java/lang/Compiler",
    "java/lang/foreign/DowncallHandle",
    "java/lang/reflect/Proxy$Dispatch",
    "java/lang/reflect/Proxy$Instance",
    "java/net/InetAddressImplFactory",
    "java/net/PlainServerSocketImpl",
    "java/net/PlainSocketImpl",
    "java/rmi/activation/Activatable",
    "java/rmi/activation/ActivationGroup",
    "java/security/AccessController$1",
    "java/util/ArrayDeque$Itr",
    "java/util/Comparator$Native",
    "java/util/Enumeration$Impl",
    "java/util/HashMap$Entry",
    "java/util/HashMap$KeyItr",
    "java/util/IteratorEnumeration",
    "java/util/LinkedList$Itr",
    "java/util/ServiceLoader$Itr",
    "java/util/TreeMap$KeyItr",
    "java/util/TreeSet$Itr",
    "java/util/concurrent/CompletedFuture",
    "java/util/concurrent/atomic/AtomicIntegerFieldUpdater$RustJvmImpl",
    "java/util/concurrent/atomic/AtomicLongFieldUpdater$RustJvmImpl",
    "java/util/concurrent/atomic/AtomicReferenceFieldUpdater$RustJvmImpl",
    "java/util/function/Consumer$AndThen",
    "java/util/function/Function$AndThen",
    "java/util/function/Function$Compose",
    "java/util/function/Function$Identity",
    "java/util/function/Predicate$$Lambda$And",
    "java/util/function/Predicate$$Lambda$Negate",
    "java/util/function/Predicate$$Lambda$Or",
    "java/util/logging/LogManager$StringEnumeration",
    "javax/net/ssl/SSLSocketInputStream",
    "javax/net/ssl/SSLSocketOutputStream",
    "jdk/internal/logger/AbstractLoggerFinder",
    "jdk/internal/misc/SharedSecrets",
    "jdk/internal/ref/BucketDirectBufferDeallocator",
    "jdk/internal/ref/DirectBufferDeallocator",
    "sun/management/Flag",
    "sun/management/HotSpotDiagnostic",
    "sun/management/OperatingSystemImpl",
    "sun/misc/Cleaner",
    "sun/misc/URLClassPath",
    "sun/nio/ch/WindowsFileDispatcherImpl",
    "sun/nio/fs/UnixWatchService",
    "sun/reflect/Reflection",
];

/// `cratonvm/…` receivers that are **stand-ins for a JDK shape**: the VM mints
/// them so that a JDK-typed caller has something to hold, and the natives on
/// them are the whole of their behaviour.
///
/// No JDK will ever declare a `cratonvm/…` name, so the §1.5 argument is
/// stronger here than for [`NO_IMAGE_JDK_RECEIVERS`] — it needs no measurement
/// at all. What still needs judgement is the split against
/// [`VM_SERVICE_RECEIVERS`], which is why this is a list and not the obvious
/// `starts_with("cratonvm/")`.
///
/// Every entry has a sibling already tagged `SyntheticStub` by hand —
/// `cratonvm/internal/UnmodifiableList` and friends, `cratonvm/internal/
/// StreamCollector`, `cratonvm/synthetic/Process*` — which is the evidence
/// that this is one family with one answer and not a new policy.
///
/// # A third way to land half a fix: re-tag the owner, leave its factory
///
/// F24-1 found it (2026-08-13); F33-1 fixed it the same day. Kept in full,
/// because the *rule* at the end is the reusable part and the next
/// `cratonvm/…` entry will need it.
///
/// [`VM_SERVICE_RECEIVERS`] warns about re-tagging without minting and minting
/// without re-tagging. The four `cratonvm/internal/ss/…$1` entries below were a
/// third shape: something else still handed out the receiver.
///
/// `shared_secrets_bridge.rs`'s `register_wp1_4_shared_secrets` sets one ambient
/// `Bridge` for the whole registrar. `register` then re-tags by receiver class,
/// and **the receiver of a factory registration is `SharedSecrets`, not the
/// object the factory returns** — so the split fell inside that one registrar:
///
/// | registered on | in a table here? | kind | `--jdk-only` |
/// |---|---|---|---|
/// | `jdk/internal/access/SharedSecrets` (the factories) | no — real JDK class | `Bridge` | **survived** |
/// | `cratonvm/internal/ss/JavaUtilJarAccess$1` (the methods) | yes, below | `SyntheticStub` | **dropped** |
///
/// So strict mode kept a native that shadows the real
/// `SharedSecrets.javaUtilJarAccess()` bytecode and returned a carrier on which
/// it had just dropped every method — and because `alloc_singleton`'s fallback
/// is `ClassId(0)`, the symptom was a **wrong-class receiver, not an error**.
/// Note `jdk/internal/misc/SharedSecrets` IS in [`NO_IMAGE_JDK_RECEIVERS`] and
/// the `jdk/internal/access/` spelling is not, correctly — only the legacy alias
/// is absent from the image — which is why the factory half was not caught here.
///
/// **THE RULE: adding a `cratonvm/…` stand-in to this table is not complete on
/// its own.** Ask what mints the receiver and what returns it to Java, and give
/// that the same kind. This table adjudicates a *class*; a factory is
/// adjudicated by the class it RETURNS.
///
/// `register_factories` now applies that rule mechanically — it asks
/// [`receiver_declared_by_no_supported_image`] about the owner rather than
/// about `SharedSecrets`, so a factory and its owner are refused together or
/// kept together, with no second list to drift. Adding a `cratonvm/internal/ss/`
/// name here is therefore now sufficient *for that family*; it is still not
/// sufficient in general, because no other minting site derives its kind this
/// way. Pinned from the other side by
/// `factory_kind_follows_the_owner_it_hands_out` and
/// `every_fabricated_factory_owner_is_a_listed_stand_in` in
/// `native-builtins/src/shared_secrets_bridge.rs`, and by
/// `no_shared_secrets_factory_outlives_the_owner_strict_mode_drops` in
/// `native-builtins/tests/stub_ratchet.rs`.
///
/// Records:
/// `docs/known-issues/jdk-only/F24-1-sharedsecrets-spellings-and-the-list-that-checked-itself-20260813.md`
/// (the finding) and
/// `docs/known-issues/jdk-only/F33-1-a-factory-and-its-owner-must-share-one-kind-20260813.md`
/// (the fix, and why "make the carriers work" was not the disposition: two of
/// the four carry no method their interface declares, on any mode).
pub const VM_MINTED_STAND_IN_RECEIVERS: &[&str] = &[
    "cratonvm/internal/ArrayListSubList",
    // `cratonvm/internal/LinkedListSnapshotListItr` was here until 2026-08-11.
    // It stands in for nobody — see the entry in [`VM_SERVICE_RECEIVERS`], and
    // note that moving it is only half a change: the other half is the mint
    // site in native-collections/src/lib.rs, and either alone is worse than
    // neither.
    "cratonvm/internal/SnapshotEnumeration",
    "cratonvm/internal/StreamChainCollector",
    "cratonvm/internal/ss/JavaIORandomAccessFileAccess$1",
    "cratonvm/internal/ss/JavaNetHttpCookieAccess$1",
    "cratonvm/internal/ss/JavaNetUriAccess$1",
    "cratonvm/internal/ss/JavaUtilJarAccess$1",
    "cratonvm/net/HttpBodyReplaySubscription",
];

/// `cratonvm/…` receivers that are **VM services**, kept `Bridge` on purpose.
///
/// Contract §11 admits "a reviewed VM service" to strict-mode dispatch
/// alongside `ACC_NATIVE` bridges and intrinsics, and `NativeKind` has no
/// variant that says so — `Bridge` is the only tag that survives
/// `NativeKind::allowed_in(JdkOnly)`. Re-tagging any of these `SyntheticStub`
/// would remove the service from `--jdk-only` rather than reclassify it:
///
/// | receiver | what strict mode would lose |
/// |---|---|
/// | `cratonvm/Instrument` | the `java.lang.instrument` bridge — every `-javaagent` |
/// | `cratonvm/internal/SystemLogger` | the VM's `System.Logger` back end |
/// | `cratonvm/Wp71JdbcSpi` | the JDBC service-provider probe |
/// | `cratonvm/tls/T27SelfTest` | the TLS self-test entry point |
/// | `cratonvm/Util`, `cratonvm/test/Util` | the regression corpus's own hooks |
/// | `cratonvm/internal/LinkedListSnapshotListItr` | `LinkedList.listIterator()`, `subList`, `sort`, and `AbstractList.equals`/`hashCode`/`indexOf` against a foreign list |
///
/// This list is not consulted by [`receiver_declared_by_no_supported_image`];
/// it exists so that "why is this one still a `Bridge`" has an answer in the
/// same file as the rule, and so a test can assert the two lists are disjoint.
/// | `java/lang/reflect/Proxy$Dispatch`, `…$Instance` | every dynamic proxy |
pub const VM_SERVICE_RECEIVERS: &[&str] = &[
    "cratonvm/Instrument",
    "cratonvm/Util",
    "cratonvm/Wp71JdbcSpi",
    // A `ListIterator` over an immutable snapshot of a native `LinkedList`, and
    // the state that iteration needs: `Object[]` snapshot at slot 0, `Int`
    // cursor at 1, backing list at 2. Contract §11's "reviewed VM service", and
    // `Bridge` is the only tag that survives `allowed_in(JdkOnly)`.
    //
    // It is here rather than in [`VM_MINTED_STAND_IN_RECEIVERS`] because it
    // stands in for nobody. It was deliberately NOT named
    // `java/util/LinkedList$ListItr` — that name resolves to the real 5-field
    // class, whose layout mangled the cursor write into the real `next:Node`
    // slot and made `next()` never advance, so `AbstractList.equals` compared
    // element 0 forever. The `cratonvm/` name is what keeps the layout ours.
    //
    // **Load-bearing pairing.** This move is inert on its own, and the mint
    // site is inert on its own, and each alone is worse than neither — the two
    // gates are independent and clearing one moves the failure rather than
    // removing it. `register()` re-tags by name here, so re-tagging without
    // minting drops nine natives strict then needs; minting without re-tagging
    // hands strict a well-formed carrier with no implementation and turns the
    // `NoClassDefFoundError` at `listIterator()` into an `UnsatisfiedLinkError`
    // at the first `hasNext()`. That is [`STRICT_STILL_FABRICATES`]'s warning
    // running in the other direction. The companion is
    // `native_ll_list_iterator` / `_idx` in native-collections/src/lib.rs.
    //
    // Retiring this entry means making the natives stop owning `LinkedList`
    // state, then dropping the `listIterator` interception — a collections
    // reclassification, not an iterator change. It does NOT take the gap off
    // the census either way: the row that names it is `native-shadows-bytecode`
    // on `java/util/LinkedList.listIterator`, keyed on the REAL class and the
    // registration, so no change to the carrier can move it. Measured in
    // docs/known-issues/jdk-only/W7-16-arraydeque-and-linkedlist-residuals.md
    "cratonvm/internal/LinkedListSnapshotListItr",
    "cratonvm/internal/SystemLogger",
    "cratonvm/test/Util",
    "cratonvm/tls/T27SelfTest",
    "java/lang/reflect/Proxy$Dispatch",
    "java/lang/reflect/Proxy$Instance",
];

/// Receivers that satisfy this module's rule and are **not** re-tagged, because
/// `--jdk-only` still creates the class and would then have no implementation
/// for it.
///
/// Such a receiver is not a different kind of registration. It is the same
/// defect as everything in [`NO_IMAGE_JDK_RECEIVERS`], waiting on the other
/// half of it: while a fabrication funnel records the `--jdk-only` violation
/// and mints the class anyway, strict mode holds an object of a class §5
/// forbids, and re-tagging its natives first turns a silent contract violation
/// into a loud `UnsatisfiedLinkError` — worse for users and no better for the
/// contract.
///
/// **To add an entry:** take a `--dump-class-origins` census under `--jdk-only`
/// (see the module docs) and list any table class that appears in it. Do not
/// wait for a corpus vector to go red: when this list held four entries, the
/// corpus reached one of them.
///
/// **To retire one:** make strict mode stop creating the class, confirm the
/// real JDK bytecode services the call, then delete the line and re-run both
/// the strict corpus and the class-origin census.
/// **Empty since 2026-08-10**, when `ClassManager::ensure_synthetic_class` was
/// deleted (`jdk-only-ensure-synthetic-class-deleted-FIXED-20260810.md`).
/// It had held `java/util/HashMap$KeyItr` and the three
/// `Atomic*FieldUpdater$RustJvmImpl` classes. With the infallible funnel gone,
/// strict mode creates none of them — a `--jdk-only` run of `RChmKeySetView`
/// now dies on `NoClassDefFoundError: java/util/HashMap$KeyItr`, which is §5
/// enforcing rather than recording — so their natives are unreachable there and
/// re-tagging them costs nothing.
///
/// Kept as an empty table rather than deleted, because the shape recurs: the
/// two halves of this defect are the registration's kind and the class's
/// existence, and moving the first without the second turns a silent §5
/// violation into an `UnsatisfiedLinkError`.
///
/// **To add an entry:** take a `--dump-class-origins` census under `--jdk-only`
/// and list any table class that appears in it. Do not wait for a corpus vector
/// to go red — when this list held four entries the corpus reached one — and do
/// not read one going red as proof either. `RChmKeySetView` reddens on
/// unmodified `dev` for the refusal above, and it was briefly misread here as
/// this table's doing. **Attribute a corpus failure by running the same vector
/// on a binary without the change**, which is the whole of what separated those
/// two readings.
pub const STRICT_STILL_FABRICATES: &[&str] = &[];

/// Whether a registration on `class_name` should be re-tagged
/// [`NativeKind::SyntheticStub`](crate::registry::NativeKind::SyntheticStub)
/// because no supported JDK image declares the receiver.
///
/// `false` for the reviewed VM services and for any receivers strict mode still
/// fabricates — the latter table is EMPTY today — both exclusions being listed
/// and reasoned in [`VM_SERVICE_RECEIVERS`] and [`STRICT_STILL_FABRICATES`].
///
/// Linear-ish: two binary searches over 49- and 8-entry tables plus one over the
/// currently-empty exclusion. This runs once per registration during
/// `SharedVm::new` (~11,900 times, at boot, off every hot path).
///
/// The table sizes above were 9 and 4 until 2026-08-12 and were wrong in a way
/// that propagated: a campaign document copied the "9" from here into a census
/// and reported 49 + 9 fabricated classes. The total it printed (57) happened to
/// be right — the tables are 49 / 8 / 9, the last being [`VM_SERVICE_RECEIVERS`]
/// — so the error survived because the number it produced was correct. A stale
/// duplicate of this doc comment sat immediately above, describing the
/// linear-scan implementation this function has not used for some time; it is
/// deleted rather than corrected, since two doc blocks on one item render as one
/// and disagreeing halves are worse than a missing half.
#[must_use]
pub fn receiver_declared_by_no_supported_image(class_name: &str) -> bool {
    // The exclusion is checked first and unconditionally. Ordering it after the
    // membership test would work today and would break the first time somebody
    // added a `cratonvm/` name to both — the failure being a strict-mode
    // UnsatisfiedLinkError far from this file.
    if STRICT_STILL_FABRICATES.binary_search(&class_name).is_ok() {
        return false;
    }
    // Cheap discriminator: every entry in the stand-in table starts with
    // `cratonvm/`, and almost no registration does, so the common case is one
    // byte compare plus one binary search.
    if class_name.starts_with("cratonvm/") {
        return VM_MINTED_STAND_IN_RECEIVERS
            .binary_search(&class_name)
            .is_ok();
    }
    if VM_SERVICE_RECEIVERS.binary_search(&class_name).is_ok() {
        return false;
    }
    NO_IMAGE_JDK_RECEIVERS.binary_search(&class_name).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`receiver_declared_by_no_supported_image`] binary-searches both tables,
    /// so an unsorted entry is not a style question: it makes the predicate
    /// answer `false` for a name that is in the list, silently.
    #[test]
    fn table_is_sorted_and_unique() {
        for (label, table) in [
            ("NO_IMAGE_JDK_RECEIVERS", NO_IMAGE_JDK_RECEIVERS),
            ("VM_MINTED_STAND_IN_RECEIVERS", VM_MINTED_STAND_IN_RECEIVERS),
            ("VM_SERVICE_RECEIVERS", VM_SERVICE_RECEIVERS),
            ("STRICT_STILL_FABRICATES", STRICT_STILL_FABRICATES),
        ] {
            let mut sorted = table.to_vec();
            sorted.sort_unstable();
            assert_eq!(table, sorted.as_slice(), "{label} is not sorted");
            sorted.dedup();
            assert_eq!(table.len(), sorted.len(), "{label} has duplicates");
        }
    }

    /// The two `cratonvm/` tables answer opposite questions about the same
    /// prefix. An overlap would make the answer depend on which one a future
    /// reader edited.
    #[test]
    fn stand_ins_and_vm_services_are_disjoint() {
        for name in VM_SERVICE_RECEIVERS {
            assert!(
                !VM_MINTED_STAND_IN_RECEIVERS.contains(name),
                "{name} is in both the stand-in and the VM-service table"
            );
            assert!(
                !receiver_declared_by_no_supported_image(name),
                "{name} is a reviewed VM service and must keep NativeKind::Bridge"
            );
        }
    }

    /// The named cases, in both directions. The positives are the families the
    /// 2026-08-10 sweep found on the deletion list; the negatives are the
    /// nearest real classes, because a prefix or substring match here would
    /// demote live bridges.
    #[test]
    fn named_receivers_answer_correctly() {
        for yes in [
            "java/util/HashMap$KeyItr",
            "java/util/concurrent/atomic/AtomicIntegerFieldUpdater$RustJvmImpl",
            "java/util/TreeSet$Itr",
            "java/util/Enumeration$Impl",
            "java/util/function/Predicate$$Lambda$And",
            "sun/nio/ch/WindowsFileDispatcherImpl",
            "cratonvm/internal/ArrayListSubList",
        ] {
            assert!(receiver_declared_by_no_supported_image(yes), "{yes}");
        }
        for no in [
            // the VM-service exclusion, asserted from the other side by
            // `strict_fabricated_receivers_are_listed_but_not_retagged`
            "java/lang/reflect/Proxy$Instance",
            // the real classes whose names the entries above are built from
            "java/util/HashMap",
            "java/util/concurrent/atomic/AtomicIntegerFieldUpdater",
            "java/lang/reflect/Proxy",
            "sun/nio/ch/UnixFileDispatcherImpl",
            "sun/nio/ch/FileDispatcherImpl",
            // a real ACC_NATIVE bridge, and the platform-variant class the
            // two-platform sweep got wrong
            "sun/nio/ch/Net",
            "jdk/net/WindowsSocketOptions",
            "sun/nio/ch/KQueuePort",
            "sun/nio/fs/PollingWatchService",
            // an unlisted cratonvm/ receiver must not be caught by the prefix
            "cratonvm/internal/UnmodifiableList",
            "cratonvm/Instrument",
        ] {
            assert!(!receiver_declared_by_no_supported_image(no), "{no}");
        }
    }

    /// The four blocked receivers are in [`NO_IMAGE_JDK_RECEIVERS`] on purpose
    /// — the image fact about them is true and the gate script should keep
    /// checking it — and the predicate must still answer `false`. An exclusion
    /// that worked by *omission* would be indistinguishable from an oversight,
    /// and the next person to re-derive the table from a census would add them
    /// back.
    #[test]
    fn strict_fabricated_receivers_are_listed_but_not_retagged() {
        for name in STRICT_STILL_FABRICATES {
            assert!(
                NO_IMAGE_JDK_RECEIVERS.contains(name)
                    || VM_MINTED_STAND_IN_RECEIVERS.contains(name),
                "{name} is excluded by omission rather than by rule"
            );
            assert!(
                !receiver_declared_by_no_supported_image(name),
                "{name} is still fabricated under --jdk-only; re-tagging it \
                 replaces a silent §5 violation with an UnsatisfiedLinkError"
            );
        }
        // Same shape for the proxy machinery, which is excluded as a VM
        // service rather than as a blocked stand-in.
        for name in [
            "java/lang/reflect/Proxy$Dispatch",
            "java/lang/reflect/Proxy$Instance",
        ] {
            assert!(NO_IMAGE_JDK_RECEIVERS.contains(&name), "{name}");
            assert!(VM_SERVICE_RECEIVERS.contains(&name), "{name}");
            assert!(!receiver_declared_by_no_supported_image(name), "{name}");
        }
    }

    /// `sun/nio/ch/KQueuePort` and `sun/nio/fs/PollingWatchService` are the two
    /// names a linux+windows sweep called "in no image" and a macOS image has.
    /// They are the reason the table is measured against six images and not
    /// four, and their absence from it is the assertion that keeps that true.
    #[test]
    fn macos_only_classes_are_not_in_the_table() {
        for name in ["sun/nio/ch/KQueuePort", "sun/nio/fs/PollingWatchService"] {
            assert!(
                !NO_IMAGE_JDK_RECEIVERS.contains(&name),
                "{name} is declared by the macOS images and must not be demoted"
            );
        }
    }
}
