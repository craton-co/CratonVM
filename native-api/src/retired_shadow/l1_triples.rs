#[allow(unused_imports)]
use super::*;

/// Lane L1 wave 1, 2026-09-10: the six `java/util` families whose state is
/// already the REAL object — 265 triples over eighteen classes.
///
/// A fifth table, for the reason the third and fourth give for existing: a
/// different METHOD, and the method is what a later reader needs at a glance.
/// Phase 3 adjudicated a PAIR of classes on a trial binary. This wave
/// adjudicated **six armed prefixes, one at a time, against a fixed 44-probe
/// subset of the tree**, and then took the whole tree and the corpus on a
/// trial binary. The lane page is
/// `docs/feature-designs/native-dispatch-architecture.md`.
///
/// # Why these six and not the other ten
///
/// `docs/feature-designs/native-dispatch-architecture.md` §3 states the rule this wave is an
/// application of: *if the VM keeps a collection's state in a Rust side
/// table, retiring the accessors hands reads to bytecode that looks at an
/// empty object.* So the question per family is not "is the probe green" but
/// "whose object is the authority", and the answer is in this crate's
/// siblings rather than in any measurement:
///
/// ```text
///   family        authority                                    verdict
///   ArrayList     real `elementData`/`size`                     retire
///   ArrayDeque    real `elements`/`head`/`tail` (slot 3 spare)   retire
///   LinkedList    `ll_set` writes `first`/`last`/`size` AND the
///                 real `LinkedList$Node` item/next/prev order    retire
///   Collections   stateless factories + the empty singletons     retire
///   Optional      real `value` (slot 0); the primitive trio use
///                 the real `isPresent`/`value` 2-field layout     retire
///   Arrays        stateless                                      retire
///   TreeMap/Set   `tm_array_table()` — a Rust side table         HELD
///                 REVISED 2026-09-12 (wave 8): the real `root` is a
///                 real `TreeMap$Entry` tree, `size` is mirrored, and
///                 the side table now has a READER. RETIRED, 156.
///   LinkedHashMap `lhm_overlay()` — a Rust side table            HELD
///   HashMap       real `table`, but 9 probes move                HELD
///   Hashtable     real `table`, but 4 probes move                HELD
///   Date/TimeZone 5 probes move                                  HELD
///   Locale        3 probes move, one truncates                   HELD
/// ```
///
/// The armed-one-prefix-at-a-time numbers, 44 probes each, base binary
/// `cratonvm-l1-base-20260910` (`origin/dev` at `7a8b79526`):
///
/// ```text
///   scope                       worse  better  probes that ASKED the dial
///   java/util/ArrayList,Arrays$ArrayList   0      1      44
///   java/util/ArrayDeque                   0      1      44
///   java/util/LinkedList                   0      1       9
///   java/util/Collections                  0      1      36
///   java/util/Optional                     0      0      15
///   java/util/Arrays                       0      0      44
///   java/util/TreeMap,TreeSet              9      1      19   <- RETIRED
///                                                              wave 8; the
///                                                              9 were a dial
///                                                              reading, and
///                                                              two binaries
///                                                              say 1
///   java/util/LinkedHashMap               11      1      18   <- held
///   java/util/HashMap                      9      0      44   <- held
///   java/util/Hashtable                    4      0      44   <- held
///   java/util/Date,TimeZone,sun/util/cal   5      0      10   <- held
///   java/util/Locale,sun/util/locale/      3      0      12   <- held
/// ```
///
/// `NullArgMsgProbe` is the "better" row in four of the six, and it is the
/// reason to read the direction rather than the movement: armed, this VM
/// starts producing the JDK's own null-argument messages instead of its
/// invented ones. `java/util/Collections` alone takes it from 26 diffs to 14.
///
/// # Precondition 4, and the two probes written for it
///
/// 265 of the 272 rows in these six prefixes were dispatched
/// (`invocations > 0`) in a UNION over per-probe
/// `--dump-native-registry --explain-jdk-only` runs of the whole 118-probe
/// tree. It was 247 until `L1Wave1Sweep.java` was written for
/// this wave — 54 rows chosen so the EMPTY answer and the right answer print
/// differently, covering the `ArrayList$SubList` sequenced surface
/// (`addFirst`/`addLast`/`getFirst`/`getLast`/`removeFirst`/`removeLast`/
/// `reversed`), both `toArray(IntFunction)` overloads, `LinkedList.isEmpty`
/// and `stream`, and `Collections$EmptyListIterator.remove`. That probe found
/// a defect on the way in: `Collections.emptyList().listIterator().remove()`
/// threw `IllegalStateException: Collections.emptyIterator(): remove() before
/// next()` where HotSpot's is message-less — a row this wave retires, so
/// yielding is the fix.
///
/// # SEVEN rows were excluded, and SIX of them had to come back
///
/// They own their slot, they are `Bridge`, their image target carries `Code`
/// — and a probe written to call them could not make them fire, in either
/// mode:
///
/// ```text
///   java/util/Collections$SetFromMap  size/isEmpty/contains/iterator/toArray
///   java/util/Arrays$ArrayList        <init>([Ljava/lang/Object;)V
///   java/util/Collections             <clinit>()V
/// ```
///
/// **The first trial binary showed why that reading was wrong.** With wave 1
/// in the table and `Arrays$ArrayList.<init>` left out, `ArrayListShadowSweep`
/// row 125 — `Arrays.asList((Object[]) null)` — regressed from
/// `THREW java.lang.NullPointerException` to `no-throw`. Real
/// `Arrays.asList` is `return new ArrayList<>(a)`, real
/// `Arrays$ArrayList.<init>` is `a = Objects.requireNonNull(array)`, and the
/// constructor's counter was 0 **because the native `asList` never reached
/// it**. Retiring the producer is what makes the consumer reachable.
///
/// So: precondition 4 is measured on the UNRETIRED binary, and a zero there
/// means "nothing reaches this today", not "nothing can". When the row that
/// was serving the producer is in the same wave, the consumer has to move
/// with it. Six rows joined on that argument —
/// `Arrays$ArrayList.<init>` behind `Arrays.asList`, and the five
/// `Collections$SetFromMap` accessors behind `Collections.newSetFromMap`.
///
/// `java/util/Collections.<clinit>()V` is the one that stays out. It has no
/// producer to retire: a `<clinit>` is reached by class initialisation,
/// which this table cannot change. Recorded rather than merely absent — the
/// same disposition Phase 3 gave `reduceEntries`.
///
/// # Four entries came OFF the held list, and here is what moved them
///
/// `the_held_collection_families_are_not_retired` named
/// `ArrayDeque.addLast`, `LinkedList.add(Object)`, `ArrayList$Itr.next` and
/// `Arrays.copyOf([Object;I)` as "state is not real" / "load-bearing FOR the
/// retirements above". Three of the four are the same correction: the state
/// became real after that list was written. `ll_set` publishes `first`,
/// `last` and `size` to the receiver's own fields and `ll_alloc_node` uses
/// the real `LinkedList$Node` slot order; `ad_ensure_capacity` keeps the
/// JDK's one-spare-slot emptiness invariant on the real
/// `elements`/`head`/`tail`; `al_itr_slots` keeps `cursor`/`lastRet`/
/// `expectedModCount` where the real `ArrayList$Itr` declares them.
/// `Arrays.copyOf` is the fourth and is different: it was held because the
/// ArrayList five depended on it, and this wave retires the dependents and
/// the dependency together, which is the only configuration that is not a
/// split store.
///
/// # Wave 2, same day: the five families the probe TREE could not ask about
///
/// Wave 1's sweep read `0 worse` for `java/util/jar/`, `java/util/zip/`,
/// `java/text/`, `java/time/`, `java/util/Stack`, `java/util/Vector`,
/// `java/util/PriorityQueue` and `java/util/ResourceBundle` — and 44 of 44,
/// 43 of 44, 44 of 44 and 43 of 44 of those probe runs were **VACUOUS**:
/// `enforcement_dial.reached` was 0, so every zero was the zero of a question
/// nobody posed. `apps/probes/L1TailSweep.java` is the question — 137 rows
/// across all eight — and it changed four of the eight verdicts:
///
/// ```text
///   scope                      worse  better  L1TailSweep delta   verdict
///   java/util/zip/                 0       1   -1  (and it un-dies)  retire
///   java/util/Stack,Vector         0       1    0                    retire
///   java/util/PriorityQueue        0       0    0  (639 yields)      retire
///   java/time/                     0       0    0  (16/17)           retire
///   java/util/ResourceBundle       0       1    0  (60/759)          HELD *
///   java/util/jar/                 1       0  +28                    HELD
///   java/text/                     1       0  +11                    HELD
/// ```
///
/// **\* `java/util/ResourceBundle` is the row where the DIAL and the TRIAL
/// BINARY disagreed, and the trial binary is right.** Armed, it was clean over
/// 51 probes. Retired, it moved five probes by ten rows, all of them the same
/// value:
///
/// ```text
///   TimeZone.getDisplayName()          HotSpot "Eastern Standard Time"
///   control                                    "Eastern Standard Time"
///   trial (ResourceBundle retired)             "Coordinated Universal Time"
///   LocaleDateTzShadowSweep 98/99      "EST"/"EDT"  ->  "UTC"/"UTC"
/// ```
///
/// This is Phase 3's §"the dial is the wrong instrument here" from the other
/// side. `CRATONVM_ENFORCE_NATIVE_SHADOW` declines at nine DISPATCH doors, and
/// a call that ORIGINATES INSIDE A NATIVE is not one of them:
/// `TimeZone.getDisplayName` is served by a native that reaches
/// `ResourceBundle` through `ctx.invoke_*`, so arming the dial left that path
/// on the native and reported clean. A registration REFUSED at `register` has
/// no native to reach, and the real `TimeZoneNameUtility` lookup then finds no
/// names and falls back to the UTC display name. **So an armed-clean prefix is
/// a candidate and never a verdict — including when the prefix is clean over
/// fifty probes.** `java/util/ResourceBundle` and `$Control` (12 rows) are
/// HELD, and the blocker is named: the locale-provider lookup the real
/// `getBundle` performs does not find the JDK's own timezone name bundles on
/// this VM.
///
/// **`java/util/zip/` is a FIX, not a neutral retirement.** On the control
/// binary, under `--real-jdk`, `new ZipFile(<a path that does not exist>)`
/// does not throw `NoSuchFileException`; it raises
/// `internal error: JarFile: cannot open ...`, which is
/// `MethodCallFailed::InternalError` — uncatchable, so `catch (Throwable)`
/// never runs and the VM exits 1 mid-probe. Armed, the probe survives it.
/// Two more rows in the same family are §1.4 defects that yielding repairs:
/// `CRC32C.update(byte[], off, len)` accepts `len` past the end of the array
/// where HotSpot throws `ArrayIndexOutOfBoundsException`, and
/// `Collections.emptyList().listIterator().remove()` (wave 1) invents a
/// message HotSpot does not have.
///
/// **`java/util/jar/` and `java/text/` are HELD and they are held for a
/// reason the tree could not have told anyone**: both were `0 worse` until a
/// probe reached them. `jar` armed takes `L1TailSweep` from 11 diffs to 39
/// and truncates it; `text` armed takes it to 22. A vacuous green is not a
/// green, and these two are the worked example.
///
/// One wave-2 row is excluded for want of precondition 4, on the same terms
/// as wave 1's: `java/util/zip/ZipFile$1.getManifestName`, a shared-secret
/// accessor shim no bytecode can name. The seven `ResourceBundle` rows that
/// would have joined it are moot — the whole family is HELD.
pub(in crate::retired_shadow) static RETIRED_SHADOW_L1_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/time/Duration",
        "parse",
        "(Ljava/lang/CharSequence;)Ljava/time/Duration;",
    ),
    ("java/time/ZoneId", "systemDefault", "()Ljava/time/ZoneId;"),
    ("java/util/ArrayDeque", "<init>", "()V"),
    ("java/util/ArrayDeque", "<init>", "(I)V"),
    (
        "java/util/ArrayDeque",
        "<init>",
        "(Ljava/util/Collection;)V",
    ),
    ("java/util/ArrayDeque", "add", "(Ljava/lang/Object;)Z"),
    (
        "java/util/ArrayDeque",
        "addAll",
        "(Ljava/util/Collection;)Z",
    ),
    ("java/util/ArrayDeque", "addFirst", "(Ljava/lang/Object;)V"),
    ("java/util/ArrayDeque", "addLast", "(Ljava/lang/Object;)V"),
    ("java/util/ArrayDeque", "clear", "()V"),
    ("java/util/ArrayDeque", "contains", "(Ljava/lang/Object;)Z"),
    ("java/util/ArrayDeque", "element", "()Ljava/lang/Object;"),
    (
        "java/util/ArrayDeque",
        "forEach",
        "(Ljava/util/function/Consumer;)V",
    ),
    ("java/util/ArrayDeque", "getFirst", "()Ljava/lang/Object;"),
    ("java/util/ArrayDeque", "getLast", "()Ljava/lang/Object;"),
    ("java/util/ArrayDeque", "isEmpty", "()Z"),
    ("java/util/ArrayDeque", "offer", "(Ljava/lang/Object;)Z"),
    (
        "java/util/ArrayDeque",
        "offerFirst",
        "(Ljava/lang/Object;)Z",
    ),
    ("java/util/ArrayDeque", "offerLast", "(Ljava/lang/Object;)Z"),
    ("java/util/ArrayDeque", "peek", "()Ljava/lang/Object;"),
    ("java/util/ArrayDeque", "peekFirst", "()Ljava/lang/Object;"),
    ("java/util/ArrayDeque", "peekLast", "()Ljava/lang/Object;"),
    ("java/util/ArrayDeque", "poll", "()Ljava/lang/Object;"),
    ("java/util/ArrayDeque", "pollFirst", "()Ljava/lang/Object;"),
    ("java/util/ArrayDeque", "pollLast", "()Ljava/lang/Object;"),
    ("java/util/ArrayDeque", "pop", "()Ljava/lang/Object;"),
    ("java/util/ArrayDeque", "push", "(Ljava/lang/Object;)V"),
    ("java/util/ArrayDeque", "remove", "()Ljava/lang/Object;"),
    ("java/util/ArrayDeque", "remove", "(Ljava/lang/Object;)Z"),
    (
        "java/util/ArrayDeque",
        "removeAll",
        "(Ljava/util/Collection;)Z",
    ),
    (
        "java/util/ArrayDeque",
        "removeFirst",
        "()Ljava/lang/Object;",
    ),
    (
        "java/util/ArrayDeque",
        "removeFirstOccurrence",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/ArrayDeque",
        "removeIf",
        "(Ljava/util/function/Predicate;)Z",
    ),
    ("java/util/ArrayDeque", "removeLast", "()Ljava/lang/Object;"),
    (
        "java/util/ArrayDeque",
        "removeLastOccurrence",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/ArrayDeque",
        "retainAll",
        "(Ljava/util/Collection;)Z",
    ),
    ("java/util/ArrayDeque", "size", "()I"),
    ("java/util/ArrayDeque", "toArray", "()[Ljava/lang/Object;"),
    ("java/util/ArrayDeque", "toString", "()Ljava/lang/String;"),
    ("java/util/ArrayList", "add", "(ILjava/lang/Object;)V"),
    ("java/util/ArrayList", "addAll", "(Ljava/util/Collection;)Z"),
    ("java/util/ArrayList", "ensureCapacity", "(I)V"),
    ("java/util/ArrayList", "equals", "(Ljava/lang/Object;)Z"),
    (
        "java/util/ArrayList",
        "forEach",
        "(Ljava/util/function/Consumer;)V",
    ),
    ("java/util/ArrayList", "hashCode", "()I"),
    ("java/util/ArrayList", "indexOf", "(Ljava/lang/Object;)I"),
    (
        "java/util/ArrayList",
        "lastIndexOf",
        "(Ljava/lang/Object;)I",
    ),
    (
        "java/util/ArrayList",
        "listIterator",
        "()Ljava/util/ListIterator;",
    ),
    (
        "java/util/ArrayList",
        "listIterator",
        "(I)Ljava/util/ListIterator;",
    ),
    ("java/util/ArrayList", "remove", "(I)Ljava/lang/Object;"),
    ("java/util/ArrayList", "remove", "(Ljava/lang/Object;)Z"),
    (
        "java/util/ArrayList",
        "removeAll",
        "(Ljava/util/Collection;)Z",
    ),
    (
        "java/util/ArrayList",
        "removeIf",
        "(Ljava/util/function/Predicate;)Z",
    ),
    (
        "java/util/ArrayList",
        "replaceAll",
        "(Ljava/util/function/UnaryOperator;)V",
    ),
    (
        "java/util/ArrayList",
        "retainAll",
        "(Ljava/util/Collection;)Z",
    ),
    (
        "java/util/ArrayList",
        "set",
        "(ILjava/lang/Object;)Ljava/lang/Object;",
    ),
    ("java/util/ArrayList", "sort", "(Ljava/util/Comparator;)V"),
    (
        "java/util/ArrayList",
        "stream",
        "()Ljava/util/stream/Stream;",
    ),
    (
        "java/util/ArrayList",
        "toArray",
        "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;",
    ),
    ("java/util/ArrayList", "toString", "()Ljava/lang/String;"),
    ("java/util/ArrayList", "trimToSize", "()V"),
    ("java/util/ArrayList$Itr", "remove", "()V"),
    (
        "java/util/ArrayList$ListItr",
        "add",
        "(Ljava/lang/Object;)V",
    ),
    ("java/util/ArrayList$ListItr", "hasNext", "()Z"),
    ("java/util/ArrayList$ListItr", "hasPrevious", "()Z"),
    (
        "java/util/ArrayList$ListItr",
        "next",
        "()Ljava/lang/Object;",
    ),
    ("java/util/ArrayList$ListItr", "nextIndex", "()I"),
    (
        "java/util/ArrayList$ListItr",
        "previous",
        "()Ljava/lang/Object;",
    ),
    ("java/util/ArrayList$ListItr", "previousIndex", "()I"),
    ("java/util/ArrayList$ListItr", "remove", "()V"),
    (
        "java/util/ArrayList$ListItr",
        "set",
        "(Ljava/lang/Object;)V",
    ),
    (
        "java/util/ArrayList$SubList",
        "add",
        "(ILjava/lang/Object;)V",
    ),
    (
        "java/util/ArrayList$SubList",
        "add",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/ArrayList$SubList",
        "addAll",
        "(ILjava/util/Collection;)Z",
    ),
    (
        "java/util/ArrayList$SubList",
        "addAll",
        "(Ljava/util/Collection;)Z",
    ),
    (
        "java/util/ArrayList$SubList",
        "addFirst",
        "(Ljava/lang/Object;)V",
    ),
    (
        "java/util/ArrayList$SubList",
        "addLast",
        "(Ljava/lang/Object;)V",
    ),
    ("java/util/ArrayList$SubList", "clear", "()V"),
    (
        "java/util/ArrayList$SubList",
        "contains",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/ArrayList$SubList",
        "containsAll",
        "(Ljava/util/Collection;)Z",
    ),
    (
        "java/util/ArrayList$SubList",
        "equals",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/ArrayList$SubList",
        "forEach",
        "(Ljava/util/function/Consumer;)V",
    ),
    (
        "java/util/ArrayList$SubList",
        "get",
        "(I)Ljava/lang/Object;",
    ),
    (
        "java/util/ArrayList$SubList",
        "getFirst",
        "()Ljava/lang/Object;",
    ),
    (
        "java/util/ArrayList$SubList",
        "getLast",
        "()Ljava/lang/Object;",
    ),
    ("java/util/ArrayList$SubList", "hashCode", "()I"),
    (
        "java/util/ArrayList$SubList",
        "indexOf",
        "(Ljava/lang/Object;)I",
    ),
    ("java/util/ArrayList$SubList", "isEmpty", "()Z"),
    (
        "java/util/ArrayList$SubList",
        "iterator",
        "()Ljava/util/Iterator;",
    ),
    (
        "java/util/ArrayList$SubList",
        "lastIndexOf",
        "(Ljava/lang/Object;)I",
    ),
    (
        "java/util/ArrayList$SubList",
        "listIterator",
        "()Ljava/util/ListIterator;",
    ),
    (
        "java/util/ArrayList$SubList",
        "listIterator",
        "(I)Ljava/util/ListIterator;",
    ),
    (
        "java/util/ArrayList$SubList",
        "parallelStream",
        "()Ljava/util/stream/Stream;",
    ),
    (
        "java/util/ArrayList$SubList",
        "remove",
        "(I)Ljava/lang/Object;",
    ),
    (
        "java/util/ArrayList$SubList",
        "remove",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/ArrayList$SubList",
        "removeAll",
        "(Ljava/util/Collection;)Z",
    ),
    (
        "java/util/ArrayList$SubList",
        "removeFirst",
        "()Ljava/lang/Object;",
    ),
    (
        "java/util/ArrayList$SubList",
        "removeIf",
        "(Ljava/util/function/Predicate;)Z",
    ),
    (
        "java/util/ArrayList$SubList",
        "removeLast",
        "()Ljava/lang/Object;",
    ),
    (
        "java/util/ArrayList$SubList",
        "replaceAll",
        "(Ljava/util/function/UnaryOperator;)V",
    ),
    (
        "java/util/ArrayList$SubList",
        "retainAll",
        "(Ljava/util/Collection;)Z",
    ),
    (
        "java/util/ArrayList$SubList",
        "reversed",
        "()Ljava/util/List;",
    ),
    (
        "java/util/ArrayList$SubList",
        "set",
        "(ILjava/lang/Object;)Ljava/lang/Object;",
    ),
    ("java/util/ArrayList$SubList", "size", "()I"),
    (
        "java/util/ArrayList$SubList",
        "sort",
        "(Ljava/util/Comparator;)V",
    ),
    (
        "java/util/ArrayList$SubList",
        "spliterator",
        "()Ljava/util/Spliterator;",
    ),
    (
        "java/util/ArrayList$SubList",
        "stream",
        "()Ljava/util/stream/Stream;",
    ),
    (
        "java/util/ArrayList$SubList",
        "subList",
        "(II)Ljava/util/List;",
    ),
    (
        "java/util/ArrayList$SubList",
        "toArray",
        "()[Ljava/lang/Object;",
    ),
    (
        "java/util/ArrayList$SubList",
        "toArray",
        "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;",
    ),
    (
        "java/util/ArrayList$SubList",
        "toArray",
        "([Ljava/lang/Object;)[Ljava/lang/Object;",
    ),
    (
        "java/util/ArrayList$SubList",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "java/util/ArrayList$SubList$1",
        "add",
        "(Ljava/lang/Object;)V",
    ),
    (
        "java/util/ArrayList$SubList$1",
        "forEachRemaining",
        "(Ljava/util/function/Consumer;)V",
    ),
    ("java/util/ArrayList$SubList$1", "hasNext", "()Z"),
    ("java/util/ArrayList$SubList$1", "hasPrevious", "()Z"),
    (
        "java/util/ArrayList$SubList$1",
        "next",
        "()Ljava/lang/Object;",
    ),
    ("java/util/ArrayList$SubList$1", "nextIndex", "()I"),
    (
        "java/util/ArrayList$SubList$1",
        "previous",
        "()Ljava/lang/Object;",
    ),
    ("java/util/ArrayList$SubList$1", "previousIndex", "()I"),
    ("java/util/ArrayList$SubList$1", "remove", "()V"),
    (
        "java/util/ArrayList$SubList$1",
        "set",
        "(Ljava/lang/Object;)V",
    ),
    (
        "java/util/Arrays",
        "asList",
        "([Ljava/lang/Object;)Ljava/util/List;",
    ),
    ("java/util/Arrays", "binarySearch", "([II)I"),
    ("java/util/Arrays", "copyOf", "([BI)[B"),
    ("java/util/Arrays", "copyOf", "([II)[I"),
    (
        "java/util/Arrays",
        "copyOf",
        "([Ljava/lang/Object;I)[Ljava/lang/Object;",
    ),
    (
        "java/util/Arrays",
        "copyOf",
        "([Ljava/lang/Object;ILjava/lang/Class;)[Ljava/lang/Object;",
    ),
    ("java/util/Arrays", "copyOfRange", "([BII)[B"),
    (
        "java/util/Arrays",
        "copyOfRange",
        "([Ljava/lang/Object;II)[Ljava/lang/Object;",
    ),
    ("java/util/Arrays", "equals", "([B[B)Z"),
    ("java/util/Arrays", "equals", "([I[I)Z"),
    ("java/util/Arrays", "fill", "([II)V"),
    (
        "java/util/Arrays",
        "fill",
        "([Ljava/lang/Object;Ljava/lang/Object;)V",
    ),
    ("java/util/Arrays", "hashCode", "([B)I"),
    ("java/util/Arrays", "hashCode", "([Ljava/lang/Object;)I"),
    ("java/util/Arrays", "sort", "([I)V"),
    ("java/util/Arrays", "sort", "([JII)V"),
    ("java/util/Arrays", "sort", "([Ljava/lang/Object;)V"),
    (
        "java/util/Arrays",
        "stream",
        "([Ljava/lang/Object;)Ljava/util/stream/Stream;",
    ),
    (
        "java/util/Arrays",
        "toString",
        "([Ljava/lang/Object;)Ljava/lang/String;",
    ),
    (
        "java/util/Arrays$ArrayList",
        "<init>",
        "([Ljava/lang/Object;)V",
    ),
    ("java/util/Arrays$ArrayList", "get", "(I)Ljava/lang/Object;"),
    ("java/util/Arrays$ArrayList", "isEmpty", "()Z"),
    ("java/util/Arrays$ArrayList", "size", "()I"),
    (
        "java/util/Arrays$ArrayList",
        "toArray",
        "()[Ljava/lang/Object;",
    ),
    (
        "java/util/Collections",
        "addAll",
        "(Ljava/util/Collection;[Ljava/lang/Object;)Z",
    ),
    (
        "java/util/Collections",
        "emptyEnumeration",
        "()Ljava/util/Enumeration;",
    ),
    (
        "java/util/Collections",
        "emptyIterator",
        "()Ljava/util/Iterator;",
    ),
    ("java/util/Collections", "emptyList", "()Ljava/util/List;"),
    (
        "java/util/Collections",
        "emptyListIterator",
        "()Ljava/util/ListIterator;",
    ),
    ("java/util/Collections", "emptyMap", "()Ljava/util/Map;"),
    ("java/util/Collections", "emptySet", "()Ljava/util/Set;"),
    (
        "java/util/Collections",
        "fill",
        "(Ljava/util/List;Ljava/lang/Object;)V",
    ),
    (
        "java/util/Collections",
        "frequency",
        "(Ljava/util/Collection;Ljava/lang/Object;)I",
    ),
    (
        "java/util/Collections",
        "max",
        "(Ljava/util/Collection;)Ljava/lang/Object;",
    ),
    (
        "java/util/Collections",
        "min",
        "(Ljava/util/Collection;)Ljava/lang/Object;",
    ),
    (
        "java/util/Collections",
        "nCopies",
        "(ILjava/lang/Object;)Ljava/util/List;",
    ),
    (
        "java/util/Collections",
        "newSetFromMap",
        "(Ljava/util/Map;)Ljava/util/Set;",
    ),
    ("java/util/Collections", "reverse", "(Ljava/util/List;)V"),
    ("java/util/Collections", "shuffle", "(Ljava/util/List;)V"),
    (
        "java/util/Collections",
        "singleton",
        "(Ljava/lang/Object;)Ljava/util/Set;",
    ),
    (
        "java/util/Collections",
        "singletonList",
        "(Ljava/lang/Object;)Ljava/util/List;",
    ),
    (
        "java/util/Collections",
        "singletonMap",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/Map;",
    ),
    ("java/util/Collections", "sort", "(Ljava/util/List;)V"),
    (
        "java/util/Collections",
        "sort",
        "(Ljava/util/List;Ljava/util/Comparator;)V",
    ),
    ("java/util/Collections", "swap", "(Ljava/util/List;II)V"),
    (
        "java/util/Collections",
        "synchronizedCollection",
        "(Ljava/util/Collection;)Ljava/util/Collection;",
    ),
    (
        "java/util/Collections",
        "synchronizedList",
        "(Ljava/util/List;)Ljava/util/List;",
    ),
    (
        "java/util/Collections",
        "synchronizedSet",
        "(Ljava/util/Set;)Ljava/util/Set;",
    ),
    (
        "java/util/Collections$EmptyEnumeration",
        "hasMoreElements",
        "()Z",
    ),
    (
        "java/util/Collections$EmptyEnumeration",
        "nextElement",
        "()Ljava/lang/Object;",
    ),
    ("java/util/Collections$EmptyIterator", "hasNext", "()Z"),
    (
        "java/util/Collections$EmptyIterator",
        "next",
        "()Ljava/lang/Object;",
    ),
    ("java/util/Collections$EmptyIterator", "remove", "()V"),
    (
        "java/util/Collections$EmptyListIterator",
        "add",
        "(Ljava/lang/Object;)V",
    ),
    ("java/util/Collections$EmptyListIterator", "hasNext", "()Z"),
    (
        "java/util/Collections$EmptyListIterator",
        "hasPrevious",
        "()Z",
    ),
    (
        "java/util/Collections$EmptyListIterator",
        "next",
        "()Ljava/lang/Object;",
    ),
    (
        "java/util/Collections$EmptyListIterator",
        "nextIndex",
        "()I",
    ),
    (
        "java/util/Collections$EmptyListIterator",
        "previous",
        "()Ljava/lang/Object;",
    ),
    (
        "java/util/Collections$EmptyListIterator",
        "previousIndex",
        "()I",
    ),
    ("java/util/Collections$EmptyListIterator", "remove", "()V"),
    (
        "java/util/Collections$EmptyListIterator",
        "set",
        "(Ljava/lang/Object;)V",
    ),
    (
        "java/util/Collections$SetFromMap",
        "contains",
        "(Ljava/lang/Object;)Z",
    ),
    ("java/util/Collections$SetFromMap", "isEmpty", "()Z"),
    (
        "java/util/Collections$SetFromMap",
        "iterator",
        "()Ljava/util/Iterator;",
    ),
    ("java/util/Collections$SetFromMap", "size", "()I"),
    (
        "java/util/Collections$SetFromMap",
        "toArray",
        "()[Ljava/lang/Object;",
    ),
    ("java/util/LinkedList", "<init>", "()V"),
    (
        "java/util/LinkedList",
        "<init>",
        "(Ljava/util/Collection;)V",
    ),
    ("java/util/LinkedList", "add", "(ILjava/lang/Object;)V"),
    ("java/util/LinkedList", "add", "(Ljava/lang/Object;)Z"),
    (
        "java/util/LinkedList",
        "addAll",
        "(ILjava/util/Collection;)Z",
    ),
    (
        "java/util/LinkedList",
        "addAll",
        "(Ljava/util/Collection;)Z",
    ),
    ("java/util/LinkedList", "addFirst", "(Ljava/lang/Object;)V"),
    ("java/util/LinkedList", "addLast", "(Ljava/lang/Object;)V"),
    ("java/util/LinkedList", "clear", "()V"),
    ("java/util/LinkedList", "contains", "(Ljava/lang/Object;)Z"),
    (
        "java/util/LinkedList",
        "descendingIterator",
        "()Ljava/util/Iterator;",
    ),
    ("java/util/LinkedList", "get", "(I)Ljava/lang/Object;"),
    ("java/util/LinkedList", "getFirst", "()Ljava/lang/Object;"),
    ("java/util/LinkedList", "getLast", "()Ljava/lang/Object;"),
    ("java/util/LinkedList", "isEmpty", "()Z"),
    ("java/util/LinkedList", "iterator", "()Ljava/util/Iterator;"),
    (
        "java/util/LinkedList",
        "listIterator",
        "()Ljava/util/ListIterator;",
    ),
    (
        "java/util/LinkedList",
        "listIterator",
        "(I)Ljava/util/ListIterator;",
    ),
    ("java/util/LinkedList", "offer", "(Ljava/lang/Object;)Z"),
    ("java/util/LinkedList", "peek", "()Ljava/lang/Object;"),
    ("java/util/LinkedList", "poll", "()Ljava/lang/Object;"),
    ("java/util/LinkedList", "pollFirst", "()Ljava/lang/Object;"),
    ("java/util/LinkedList", "pollLast", "()Ljava/lang/Object;"),
    ("java/util/LinkedList", "remove", "(I)Ljava/lang/Object;"),
    ("java/util/LinkedList", "remove", "(Ljava/lang/Object;)Z"),
    (
        "java/util/LinkedList",
        "removeFirst",
        "()Ljava/lang/Object;",
    ),
    (
        "java/util/LinkedList",
        "removeFirstOccurrence",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/LinkedList",
        "removeIf",
        "(Ljava/util/function/Predicate;)Z",
    ),
    ("java/util/LinkedList", "removeLast", "()Ljava/lang/Object;"),
    (
        "java/util/LinkedList",
        "removeLastOccurrence",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/LinkedList",
        "set",
        "(ILjava/lang/Object;)Ljava/lang/Object;",
    ),
    ("java/util/LinkedList", "size", "()I"),
    (
        "java/util/LinkedList",
        "spliterator",
        "()Ljava/util/Spliterator;",
    ),
    (
        "java/util/LinkedList",
        "stream",
        "()Ljava/util/stream/Stream;",
    ),
    ("java/util/LinkedList", "toArray", "()[Ljava/lang/Object;"),
    (
        "java/util/LinkedList",
        "toArray",
        "([Ljava/lang/Object;)[Ljava/lang/Object;",
    ),
    ("java/util/LinkedList", "toString", "()Ljava/lang/String;"),
    (
        "java/util/LinkedList$ListItr",
        "add",
        "(Ljava/lang/Object;)V",
    ),
    ("java/util/LinkedList$ListItr", "hasNext", "()Z"),
    ("java/util/LinkedList$ListItr", "hasPrevious", "()Z"),
    (
        "java/util/LinkedList$ListItr",
        "next",
        "()Ljava/lang/Object;",
    ),
    ("java/util/LinkedList$ListItr", "nextIndex", "()I"),
    (
        "java/util/LinkedList$ListItr",
        "previous",
        "()Ljava/lang/Object;",
    ),
    ("java/util/LinkedList$ListItr", "previousIndex", "()I"),
    ("java/util/LinkedList$ListItr", "remove", "()V"),
    (
        "java/util/LinkedList$ListItr",
        "set",
        "(Ljava/lang/Object;)V",
    ),
    ("java/util/Optional", "empty", "()Ljava/util/Optional;"),
    ("java/util/Optional", "equals", "(Ljava/lang/Object;)Z"),
    (
        "java/util/Optional",
        "filter",
        "(Ljava/util/function/Predicate;)Ljava/util/Optional;",
    ),
    (
        "java/util/Optional",
        "flatMap",
        "(Ljava/util/function/Function;)Ljava/util/Optional;",
    ),
    ("java/util/Optional", "get", "()Ljava/lang/Object;"),
    ("java/util/Optional", "hashCode", "()I"),
    (
        "java/util/Optional",
        "ifPresent",
        "(Ljava/util/function/Consumer;)V",
    ),
    (
        "java/util/Optional",
        "ifPresentOrElse",
        "(Ljava/util/function/Consumer;Ljava/lang/Runnable;)V",
    ),
    ("java/util/Optional", "isEmpty", "()Z"),
    ("java/util/Optional", "isPresent", "()Z"),
    (
        "java/util/Optional",
        "map",
        "(Ljava/util/function/Function;)Ljava/util/Optional;",
    ),
    (
        "java/util/Optional",
        "of",
        "(Ljava/lang/Object;)Ljava/util/Optional;",
    ),
    (
        "java/util/Optional",
        "ofNullable",
        "(Ljava/lang/Object;)Ljava/util/Optional;",
    ),
    (
        "java/util/Optional",
        "or",
        "(Ljava/util/function/Supplier;)Ljava/util/Optional;",
    ),
    (
        "java/util/Optional",
        "orElse",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/Optional",
        "orElseGet",
        "(Ljava/util/function/Supplier;)Ljava/lang/Object;",
    ),
    ("java/util/Optional", "orElseThrow", "()Ljava/lang/Object;"),
    (
        "java/util/Optional",
        "orElseThrow",
        "(Ljava/util/function/Supplier;)Ljava/lang/Object;",
    ),
    (
        "java/util/Optional",
        "stream",
        "()Ljava/util/stream/Stream;",
    ),
    ("java/util/Optional", "toString", "()Ljava/lang/String;"),
    (
        "java/util/OptionalDouble",
        "empty",
        "()Ljava/util/OptionalDouble;",
    ),
    ("java/util/OptionalDouble", "getAsDouble", "()D"),
    (
        "java/util/OptionalDouble",
        "ifPresent",
        "(Ljava/util/function/DoubleConsumer;)V",
    ),
    ("java/util/OptionalDouble", "isPresent", "()Z"),
    (
        "java/util/OptionalDouble",
        "of",
        "(D)Ljava/util/OptionalDouble;",
    ),
    ("java/util/OptionalDouble", "orElse", "(D)D"),
    (
        "java/util/OptionalInt",
        "empty",
        "()Ljava/util/OptionalInt;",
    ),
    ("java/util/OptionalInt", "getAsInt", "()I"),
    (
        "java/util/OptionalInt",
        "ifPresent",
        "(Ljava/util/function/IntConsumer;)V",
    ),
    ("java/util/OptionalInt", "isPresent", "()Z"),
    ("java/util/OptionalInt", "of", "(I)Ljava/util/OptionalInt;"),
    ("java/util/OptionalInt", "orElse", "(I)I"),
    (
        "java/util/OptionalLong",
        "empty",
        "()Ljava/util/OptionalLong;",
    ),
    ("java/util/OptionalLong", "getAsLong", "()J"),
    (
        "java/util/OptionalLong",
        "ifPresent",
        "(Ljava/util/function/LongConsumer;)V",
    ),
    ("java/util/OptionalLong", "isPresent", "()Z"),
    (
        "java/util/OptionalLong",
        "of",
        "(J)Ljava/util/OptionalLong;",
    ),
    ("java/util/OptionalLong", "orElse", "(J)J"),
    ("java/util/PriorityQueue", "<init>", "()V"),
    ("java/util/PriorityQueue", "<init>", "(I)V"),
    (
        "java/util/PriorityQueue",
        "<init>",
        "(Ljava/util/Collection;)V",
    ),
    (
        "java/util/PriorityQueue",
        "<init>",
        "(Ljava/util/Comparator;)V",
    ),
    ("java/util/PriorityQueue", "add", "(Ljava/lang/Object;)Z"),
    ("java/util/PriorityQueue", "clear", "()V"),
    (
        "java/util/PriorityQueue",
        "contains",
        "(Ljava/lang/Object;)Z",
    ),
    ("java/util/PriorityQueue", "isEmpty", "()Z"),
    (
        "java/util/PriorityQueue",
        "iterator",
        "()Ljava/util/Iterator;",
    ),
    ("java/util/PriorityQueue", "offer", "(Ljava/lang/Object;)Z"),
    ("java/util/PriorityQueue", "peek", "()Ljava/lang/Object;"),
    ("java/util/PriorityQueue", "poll", "()Ljava/lang/Object;"),
    ("java/util/PriorityQueue", "remove", "(Ljava/lang/Object;)Z"),
    ("java/util/PriorityQueue", "size", "()I"),
    (
        "java/util/PriorityQueue",
        "toArray",
        "()[Ljava/lang/Object;",
    ),
    (
        "java/util/PriorityQueue",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/util/PriorityQueue$Itr", "hasNext", "()Z"),
    (
        "java/util/PriorityQueue$Itr",
        "next",
        "()Ljava/lang/Object;",
    ),
    ("java/util/PriorityQueue$Itr", "remove", "()V"),
    ("java/util/Stack", "<init>", "()V"),
    ("java/util/Stack", "add", "(Ljava/lang/Object;)Z"),
    ("java/util/Stack", "clear", "()V"),
    ("java/util/Stack", "contains", "(Ljava/lang/Object;)Z"),
    ("java/util/Stack", "empty", "()Z"),
    ("java/util/Stack", "get", "(I)Ljava/lang/Object;"),
    ("java/util/Stack", "indexOf", "(Ljava/lang/Object;)I"),
    ("java/util/Stack", "isEmpty", "()Z"),
    ("java/util/Stack", "iterator", "()Ljava/util/Iterator;"),
    ("java/util/Stack", "peek", "()Ljava/lang/Object;"),
    ("java/util/Stack", "pop", "()Ljava/lang/Object;"),
    (
        "java/util/Stack",
        "push",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    ("java/util/Stack", "remove", "(I)Ljava/lang/Object;"),
    ("java/util/Stack", "search", "(Ljava/lang/Object;)I"),
    (
        "java/util/Stack",
        "set",
        "(ILjava/lang/Object;)Ljava/lang/Object;",
    ),
    ("java/util/Stack", "size", "()I"),
    ("java/util/Stack", "toArray", "()[Ljava/lang/Object;"),
    ("java/util/Stack", "toString", "()Ljava/lang/String;"),
    ("java/util/Vector", "addAll", "(Ljava/util/Collection;)Z"),
    ("java/util/zip/CRC32", "updateBytes", "(I[BII)I"),
    ("java/util/zip/CRC32C", "<init>", "()V"),
    ("java/util/zip/CRC32C", "getValue", "()J"),
    ("java/util/zip/CRC32C", "reset", "()V"),
    ("java/util/zip/CRC32C", "update", "(I)V"),
    ("java/util/zip/CRC32C", "update", "([B)V"),
    ("java/util/zip/CRC32C", "update", "([BII)V"),
    (
        "java/util/zip/ZipEntry",
        "setComment",
        "(Ljava/lang/String;)V",
    ),
    ("java/util/zip/ZipFile", "<init>", "(Ljava/io/File;)V"),
    ("java/util/zip/ZipFile", "<init>", "(Ljava/lang/String;)V"),
    ("java/util/zip/ZipFile", "close", "()V"),
    (
        "java/util/zip/ZipFile",
        "entries",
        "()Ljava/util/Enumeration;",
    ),
    (
        "java/util/zip/ZipFile",
        "getComment",
        "()Ljava/lang/String;",
    ),
    (
        "java/util/zip/ZipFile",
        "getEntry",
        "(Ljava/lang/String;)Ljava/util/zip/ZipEntry;",
    ),
    (
        "java/util/zip/ZipFile",
        "getInputStream",
        "(Ljava/util/zip/ZipEntry;)Ljava/io/InputStream;",
    ),
    ("java/util/zip/ZipFile", "getName", "()Ljava/lang/String;"),
    ("java/util/zip/ZipFile", "size", "()I"),
    (
        "java/util/zip/ZipFile",
        "stream",
        "()Ljava/util/stream/Stream;",
    ),
];

/// Lane 1 wave 3 — `java/util/HashMap` and its six view/iterator classes, the
/// 98 §1.4 shadows the census calls bucket A or B.
///
/// ## Why this family was HELD until now, and what the hold turned out to be
///
/// The 2026-09-10 reading was "the state IS real but nine probes move", and
/// the 2026-09-11 reading narrowed it to ONE observable:
/// `hashMap.entrySet().toArray()` answered a zero-length array where HotSpot
/// answers three, while `size()`, the entry ITERATOR, `forEach`, `stream`,
/// `spliterator` and both other views were already exact. `keySet().toArray()`
/// and `values().toArray()` were exact too, which is what made it look like a
/// defect in the entry view specifically.
///
/// It is not. `L1EntrySetRouteProbe.java` asks the one question
/// that separates the two routes that can produce that array — what happens
/// to a TYPED destination:
///
/// ```text
///   HotSpot            entrySet().toArray(new String[0])  ArrayStoreException
///   dial-armed         entrySet().toArray(new String[0])  [0]java.lang.String
/// ```
///
/// Real `AbstractCollection.toArray(T[])` `aastore`s each element and so MUST
/// throw for three `Map.Entry`s and a `String[]`; it cannot throw for an empty
/// walk. A quiet `String[0]` therefore means a NATIVE answered and believed the
/// view was empty. `CRATONVM_DBG_TOARRAY` names it:
///
/// ```text
///   [DBG_TOARRAY] native_al_to_array (0-arg) HIT nargs=1
///   [DBG_TOARRAY] al_or_collection_elements recv=java/util/HashMap$EntrySet
///                 heuristic_len=0 nulls=0 suspect=false
///   WARN zgc real: field index OOB index=1 num_slots=1 op="get"   (x10)
/// ```
///
/// and the chain is:
///
/// 1. the dial's prefix `java/util/HashMap` also covers `$EntrySet`, so
///    `entrySet()` yields and hands back the image's own `HashMap$EntrySet` —
///    `this$0` set, identity stable, the map's own `entrySet` field populated,
///    all three verified against HotSpot;
/// 2. that object has ONE slot, and `hs_map_slot` puts a view carrier's
///    backing at `class_num_total_fields` — slot 1 — so every read is out of
///    bounds and `hs_backing_map` is empty (the ten `zgc` warnings are that
///    read);
/// 3. `toArray` on it resolves up to `java/util/AbstractCollection`, which is
///    NOT armed, so `native_al_to_array` fires;
/// 4. its documented fallback for an unmodelled layout is "ask the receiver's
///    own `size()`, and walk the real `iterator()` if it is non-zero" — and
///    that question is asked from INSIDE a native, where no dispatch door
///    exists. So it reaches `native_hs_size` on `HashMap$EntrySet`, which finds
///    no backing, tries `try_delegate_real_collection`, and that helper's
///    `invoke_special` re-finds the SAME native, trips its own re-entrancy
///    guard and returns the sentinel. `real_size == 0`, no walk, `[0]`.
///
/// **So the dial's nine red rows were a dial artefact, and the ops page's rule
/// cuts both ways: an armed sweep is not a verdict when it is GREEN, and it is
/// not a verdict when it is RED either.** Retirement does not decline at a
/// door — it removes the registration, so step 4's question reaches real
/// bytecode and answers 3. Measured, not argued: the trial binary takes
/// `L1MapFamilySweep.java` from 9 diffs to 0.
///
/// Arming `AbstractCollection`, `AbstractSet`, `AbstractMap`, `Set`,
/// `Collection` and `Map` alongside changes nothing, and now there is a reason
/// rather than a shrug: the door that would have to decline is not on the
/// `toArray` call at all, it is on the `size()` call the native makes, and that
/// call has no door.
///
/// ## What the family is
///
/// 98 triples over seven classes, every one bucket A or B:
///
/// ```text
///   java/util/HashMap                 A 30   B 3
///   java/util/HashMap$EntrySet        A  7   B 14
///   java/util/HashMap$KeySet          A  9   B 12
///   java/util/HashMap$Values          A  8   B  6
///   java/util/HashMap$EntryIterator   A  1   B  2
///   java/util/HashMap$KeyIterator     A  1   B  2
///   java/util/HashMap$ValueIterator   A  1   B  2
///   java/util/HashMap$Node            A  1
/// ```
///
/// They move as a SET and cannot move any other way. `hs_map_slot` puts a view
/// carrier's backing past the class's declared fields, `key_itr_carrier_for`
/// mints `HashMap$KeyIterator` for the view's cursor, and `map_state` reads the
/// map's own bucket array — retire the map and keep the views and the views
/// read a backing nothing fills; retire the views and keep the map and the
/// map's own `keySet()` native mints a carrier whose natives are gone.
///
/// ## Precondition 4 is measured, not waived
///
/// Three earlier probe runs reached 29 of the 98. `apps/probes/
/// L1MapFamilySweep.java` — 142 rows, written for this wave — reaches the rest
/// through ordinary Java: every constructor, every default-method override,
/// both iterator `remove()` contracts, `Node.setValue` through a detached
/// entry, serialization round-trip, comodification, a 100-entry resize and a
/// 12-way hash collision chain.
///
/// ## What retiring it REPAIRS
///
/// The nine rows above are §1.4 defects that yielding fixes, and three of them
/// are silent wrong answers rather than throws — `entrySet().toArray()`,
/// `new ArrayList<>(entrySet())` and `new HashSet<>(entrySet())` all read
/// EMPTY for a three-entry map on the armed binary. The unarmed binary is also
/// worse than HotSpot on five reflective rows this wave fixes (see
/// `L1MapFieldProbe.java`): `new HashMap<>()` + three puts leaves
/// `threshold = 0` where HotSpot has 12, `new HashMap<>(64)` leaves
/// `threshold = 64` where HotSpot has 48, and the copy constructor and
/// `new HashMap<>(Map.of(..))` leave `table = [16]java.lang.Object` — an
/// UNTYPED array where HotSpot has a typed `HashMap$Node[]` — with
/// `loadFactor = 0.0`.
///
/// `java/util/LinkedHashMap` is NOT in this table even though it is a
/// `HashMap` subclass: its entries live in `lhm_overlay()`, a Rust side table
/// the real bodies cannot read, and that is §10 item 2's own change.
/// `java/util/Hashtable` is not here either — its fields already match HotSpot
/// unarmed and its four moving probes are a separate question.
pub(in crate::retired_shadow) static RETIRED_SHADOW_L1_HM_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/util/HashMap", "<init>", "()V"),
    ("java/util/HashMap", "<init>", "(I)V"),
    ("java/util/HashMap", "<init>", "(IF)V"),
    ("java/util/HashMap", "<init>", "(Ljava/util/Map;)V"),
    ("java/util/HashMap", "clear", "()V"),
    (
        "java/util/HashMap",
        "computeIfAbsent",
        "(Ljava/lang/Object;Ljava/util/function/Function;)Ljava/lang/Object;",
    ),
    ("java/util/HashMap", "containsKey", "(Ljava/lang/Object;)Z"),
    (
        "java/util/HashMap",
        "containsValue",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/HashMap",
        "forEach",
        "(Ljava/util/function/BiConsumer;)V",
    ),
    (
        "java/util/HashMap",
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/HashMap",
        "getOrDefault",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    ("java/util/HashMap", "isEmpty", "()Z"),
    (
        "java/util/HashMap",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    ("java/util/HashMap", "putAll", "(Ljava/util/Map;)V"),
    (
        "java/util/HashMap",
        "putIfAbsent",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/HashMap",
        "remove",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/HashMap",
        "remove",
        "(Ljava/lang/Object;Ljava/lang/Object;)Z",
    ),
    (
        "java/util/HashMap",
        "replace",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/HashMap",
        "replace",
        "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Z",
    ),
    ("java/util/HashMap", "size", "()I"),
    ("java/util/HashMap", "toString", "()Ljava/lang/String;"),
];

/// Lane 1 wave 4 — the 29 `java/util/jar/` and `java/text/` rows that a dial
/// BISECTION cleared, once the whole-prefix red was traced to one class each.
///
/// ## §7's two vacuous greens were also two unbisected reds
///
/// The 2026-09-10 revision of the lane page held both families because
/// `apps/probes/L1TailSweep.java` turned each prefix red — jar `+28`, text
/// `+11` — after a 44-probe subset had called both `0 worse` with the dial
/// never engaging. That was the right call and it stopped there: "neither has
/// been bisected to a method".
///
/// Bisected on 2026-09-11 against `cratonvm-l1hm-base-20260911`, one class at
/// a time, scored on `L1TailSweep` (base 13 diffs), with the dial's own
/// engagement printed beside each row so a vacuous arm cannot pass as a green:
///
/// ```text
///   java/util/jar/                 +34 WORSE   reached=146
///     java/util/jar/JarFile        +34 WORSE   reached=4    <- the whole red
///     java/util/jar/JarEntry        +0 same    reached=16
///     java/util/jar/Manifest        +0 same    reached=23
///     java/util/jar/Attributes      +0 same    reached=156
///     java/util/jar/Attributes$Name +0 same    reached=127
///   java/text/                     +16 WORSE   reached=4
///     java/text/BreakIterator      +16 WORSE   reached=4    <- the whole red
///     java/text/ParseException      +0 same    reached=1
///     java/text/Normalizer          +0 same    reached=10
///     java/text/DateFormat          +0 same    reached=0    <- VACUOUS
/// ```
///
/// Each family's red is ONE class, and `java/util/jar/JarFile` is the same
/// class wave 2 already had to retire three defect rows out of. Those two
/// stay `Bridge` and keep their prefixes off this table; `DateFormat`'s single
/// registration is excluded for the reason §7 exists — its green said nothing.
///
/// ## What yielding REPAIRS: nine of thirteen rows, measured
///
/// `apps/probes/L1JarTextSweep.java` (87 rows, written for this wave) is
/// thirteen rows out from HotSpot 25.0.4+7 on the control. Armed on this
/// table's five classes together — 1,119 door engagements, so not a vacuous
/// arm — it is **four**: nine repaired, none made worse. Every one of the
/// thirteen is a §1.4 defect, the native answering where the image's own
/// body throws or throwing where it answers:
///
/// ```text
///   A.getValue.nullName    HotSpot NullPointerException   control null
///   A.put.rejectsString    HotSpot ClassCastException     control null
///   N.ctor.empty           HotSpot IllegalArgumentException  control ""
///   N.ctor.null            HotSpot NullPointerException   control null
///   N.ctor.illegalChar     HotSpot IllegalArgumentException  control "bad name"
///   N.ctor.tooLong         HotSpot IllegalArgumentException  control 71
///   M.ctorStream.garbage   HotSpot 0                      control IOException
///   M.ctorStream.null      HotSpot NPE "this.in is null"  control NPE, own text
///   M.ctorCopy.null        HotSpot NullPointerException   control 0
///   E.ctorName.null        HotSpot NullPointerException   control null
///   E.attributesFromJar    HotSpot "section-value"        control null
///   P.printStackTrace.writer / .stream   two more, same shape
/// ```
///
/// `Attributes$Name`'s constructor is the sharpest of them: the image
/// validates the header name (non-empty, ≤ 70 characters, `[0-9A-Za-z_-]`
/// only) and this VM validated nothing at all, so `new Attributes.Name("bad
/// name")` produced a Name that can never appear in a real manifest.
/// `E.attributesFromJar` is the only one that is a wrong VALUE rather than a
/// missing throw: a jar's per-entry manifest section was invisible.
///
/// The four that remain are not this table's to fix, and saying which is the
/// point of measuring them:
///
/// * `E.attributesFromJar` — `java/util/jar/JarFile`'s, which stays `Bridge`;
/// * `M.ctorStream.null` — the NPE is raised at the right place with the
///   right type and the message lacks its `because "this.in" is null`
///   clause, which is the helpful-NPE-message gap and not a jar defect;
/// * the two `P.printStackTrace` rows — see below.
///
/// ## `java/text/ParseException` was in this table and came out
///
/// It is the wave's own refusal, on its own measurement. Armed alone it is
/// `+0` — it repairs nothing — and two of its rows trade one wrong answer
/// for another:
///
/// ```text
///   e.setStackTrace(new StackTraceElement[0]); e.printStackTrace(w)
///     HotSpot   java.text.ParseException: bad
///     control   (wrong, one way)
///     armed     java.text.ParseException: bad
///                 at java.text.ParseException.<init>(ParseException.java:64)
///                 at ... six more frames
/// ```
///
/// Thirteen of its fourteen registrations are `java/lang/Throwable`'s
/// inherited surface, so what the yield exposes is that this VM's `Throwable`
/// model does not read back a `stackTrace` array that BYTECODE wrote — a
/// `Throwable` defect that a `java/text/` retirement merely made visible.
/// `printStackTrace` is called by too much real code to change its output for
/// no repair, so the class stays and the finding is written down instead.
///
/// ## Why these five and not the prefix
///
/// `java/util/jar/JarFile` is the family's producer and it stays; that is not
/// a half-retirement of the kind wave 3 hit, because `JarEntry`, `Manifest`
/// and `Attributes` are VALUES a `JarFile` hands out rather than carriers the
/// VM mints under a borrowed class name. The discriminator is whether a
/// SURVIVING native can be handed an object real bytecode built: for wave 3's
/// `HashMap$KeyIterator` it could, and did; here the natives that survive are
/// `JarFile`'s own and they are handed `JarFile`s.
pub(in crate::retired_shadow) static RETIRED_SHADOW_L1_JT_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/text/Normalizer",
        "isNormalized",
        "(Ljava/lang/CharSequence;Ljava/text/Normalizer$Form;)Z",
    ),
    (
        "java/text/Normalizer",
        "normalize",
        "(Ljava/lang/CharSequence;Ljava/text/Normalizer$Form;)Ljava/lang/String;",
    ),
    ("java/util/jar/Attributes", "<init>", "()V"),
    ("java/util/jar/Attributes", "<init>", "(I)V"),
    (
        "java/util/jar/Attributes",
        "containsKey",
        "(Ljava/lang/Object;)Z",
    ),
    ("java/util/jar/Attributes", "entrySet", "()Ljava/util/Set;"),
    (
        "java/util/jar/Attributes",
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/jar/Attributes",
        "getValue",
        "(Ljava/lang/String;)Ljava/lang/String;",
    ),
    (
        "java/util/jar/Attributes",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/jar/Attributes",
        "putValue",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
    ),
    ("java/util/jar/Attributes", "size", "()I"),
    (
        "java/util/jar/Attributes$Name",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/util/jar/Attributes$Name",
        "equals",
        "(Ljava/lang/Object;)Z",
    ),
    ("java/util/jar/Attributes$Name", "hashCode", "()I"),
    (
        "java/util/jar/Attributes$Name",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/util/jar/JarEntry", "<init>", "(Ljava/lang/String;)V"),
    (
        "java/util/jar/JarEntry",
        "getComment",
        "()Ljava/lang/String;",
    ),
    ("java/util/jar/JarEntry", "getCompressedSize", "()J"),
    ("java/util/jar/JarEntry", "getMethod", "()I"),
    ("java/util/jar/JarEntry", "getName", "()Ljava/lang/String;"),
    ("java/util/jar/JarEntry", "getSize", "()J"),
    ("java/util/jar/JarEntry", "isDirectory", "()Z"),
    ("java/util/jar/Manifest", "<init>", "()V"),
    (
        "java/util/jar/Manifest",
        "<init>",
        "(Ljava/io/InputStream;)V",
    ),
    (
        "java/util/jar/Manifest",
        "<init>",
        "(Ljava/io/InputStream;Ljava/lang/String;)V",
    ),
    (
        "java/util/jar/Manifest",
        "<init>",
        "(Ljava/util/jar/JarVerifier;Ljava/io/InputStream;Ljava/lang/String;)V",
    ),
    (
        "java/util/jar/Manifest",
        "<init>",
        "(Ljava/util/jar/Manifest;)V",
    ),
    ("java/util/jar/Manifest", "getEntries", "()Ljava/util/Map;"),
    (
        "java/util/jar/Manifest",
        "getMainAttributes",
        "()Ljava/util/jar/Attributes;",
    ),
];

/// Lane 1, wave 5 — `sun/util/calendar/ZoneInfoFile`, the one row of the
/// Date/TimeZone/Locale family that a bisection left standing.
///
/// ## Why two rows and not the forty
///
/// §10 item 6 of the lane page holds `Date`/`TimeZone`/`sun/util/calendar/`
/// (40) and `Locale` + providers (35) together, and the reason they were held
/// together was that nobody had asked them separately. Armed one class at a
/// time on `LocaleDateTzShadowSweep` (base 2 diffs), 2026-09-11:
///
/// ```text
///   java/util/Locale                       +117 WORSE  rc 1   reached=58
///   sun/util/calendar/ZoneInfo               +4 WORSE         reached=5723
///   java/util/TimeZone                       +2 WORSE         reached=53
///   java/util/Currency                       +2 WORSE         reached=4
///   sun/util/calendar/ZoneInfoFile           +0 same          reached=3792
///   java/util/Date                           +0 same          reached=0   VACUOUS
///   sun/util/locale/provider/CalendarDataUtility      +0      reached=0   VACUOUS
///   sun/util/locale/provider/JRELocaleProviderAdapter +0      reached=0   VACUOUS
///   sun/util/locale/provider/LocaleResources          +0      reached=0   VACUOUS
///   sun/util/resources/Bundles                        +0      reached=0   VACUOUS
///   sun/util/resources/LocaleData                     +0      reached=0   VACUOUS
/// ```
///
/// Six of the seven `+0` rows are §7's vacuity trap wearing a green coat: the
/// probe never asks those classes anything, so `reached == 0` and the row says
/// NOTHING. `ZoneInfoFile` is the one `+0` that means something — 3,792 door
/// engagements and no diff — and it is the only row here.
///
/// **A green dial arm is a candidate, not a verdict, and so is a red one.**
/// This wave's own sibling finding is that `java/util/HashMap` sat held for two
/// page revisions on nine rows a trial binary reads as zero
/// (`RETIRED_SHADOW_L1_HM_TRIPLES`), and the converse — an armed arm that
/// agrees with base is exactly what a LEAKED dial row also looks like. So the
/// 3,792 buys this table a place in the queue, not a landing: the acceptance is
/// a trial binary against the whole probe tree, per the four preconditions in
/// the ops page §7.
///
/// ## What the two rows are
///
/// Both are registered in `native-builtins/src/lib.rs` and both answer with
/// `alloc_synth_timezone` — a `sun/util/calendar/ZoneInfo` this VM builds from
/// its own tzdb parse, with `ID`/`rawOffset`/`dstSavings` populated. The real
/// bytecode reads `tzdb.dat` out of the image instead. They are a pair on
/// purpose: `getZoneInfo` is the public entry point, `getZoneInfo0` the private
/// one it delegates to, and retiring one without the other would leave a
/// synthesised object being handed to real bytecode or the reverse — the
/// half-retirement shape wave 3 measured on `HashMap`'s iterators.
///
/// ## What is NOT here, and why the prefix is narrow
///
/// `sun/util/locale/provider/` and `sun/util/resources/` are the five vacuous
/// rows above, and they are all one blocker: `LocaleResources` answers `null`
/// for the class-based bundle families (`getBreakIteratorInfo`,
/// `getDateTimePattern`), which is what makes `java/text/BreakIterator`'s
/// yield throw `AbstractMethodError` and what BUG-15 in
/// `vm/src/vm/vm_exec.rs` pins a native over. That is one engineering front
/// under four of the lane's remaining items, and it is §10's, not this table's.
/// Admitting `sun/util/` as a prefix would put all of it one binary search from
/// a future table for no gain today, so the prefix names this class alone.
///
/// ## ADDENDUM, wave 6 (2026-09-11): the six vacuous rows are vacuous no more
///
/// A `reached == 0` row is a request for a workload, and wave 6 wrote one:
/// `apps/probes/L1LocaleProviderWorkload`, 147 rows driving every public API
/// whose real-JDK implementation goes through these classes. Armed one class
/// per process against the wave-6 control binary, the six rows above now
/// read:
///
/// ```text
///   java/util/Date                                     +0   reached=14
///   sun/util/locale/provider/CalendarDataUtility      +12   reached=108
///   sun/util/locale/provider/JRELocaleProviderAdapter   +0   reached=8
///   sun/util/locale/provider/LocaleResources          +12   reached=254
///   sun/util/resources/Bundles                          +0   reached=0
///   sun/util/resources/LocaleData                       +0   reached=21
/// ```
///
/// and four neighbours the same run priced for the first time:
///
/// ```text
///   sun/util/calendar/          +0   reached=411   (ZoneInfoFile already out)
///   java/util/TimeZone          +0   reached=196
///   java/util/Currency         +38   reached=75
///   java/util/Locale            +6   reached=16156
/// ```
///
/// So the paragraph above is HALF wrong and the half matters:
/// `LocaleResources` and `CalendarDataUtility` are genuinely blocked (+12
/// each, on 108 and 254 engagements), and `Currency` and `Locale` are worse
/// still. But `Date`, `JRELocaleProviderAdapter`, `LocaleData`, `TimeZone` and
/// the rest of `sun/util/calendar/` are CANDIDATES with engagement behind
/// them, and the sentence that called all five one blocker was reasoning from
/// an empty set. `sun/util/resources/Bundles` is the one row still at
/// `reached=0`, and it stays a non-answer.
///
/// A candidate is still not a verdict -- that is this table's own standing
/// warning, and none of these four has had a trial binary yet.
pub(in crate::retired_shadow) static RETIRED_SHADOW_L1_ZI_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "sun/util/calendar/ZoneInfoFile",
        "getZoneInfo",
        "(Ljava/lang/String;)Lsun/util/calendar/ZoneInfo;",
    ),
    (
        "sun/util/calendar/ZoneInfoFile",
        "getZoneInfo0",
        "(Ljava/lang/String;)Lsun/util/calendar/ZoneInfo;",
    ),
];

/// Lane 1 wave 6, 2026-09-11: `java/text/BreakIterator`, all seventeen.
///
/// The family the lane page held for two waves as "one throw, and it is the
/// locale provider's". It was, and this is the other end of that thread.
///
/// # Why seventeen and not a subset
///
/// Because the blocker was never per-method. `java.text.BreakIterator` is
/// ABSTRACT: everything the JDK hands back is a `sun.text.RuleBasedBreakIterator`,
/// a `sun.text.DictionaryBasedBreakIterator` or a
/// `BreakIteratorProviderImpl$GraphemeBreakIterator`, and once the real chain
/// builds one, every one of the seventeen is answered by that object's own
/// bytecode. Retiring half would leave the fabricated carrier reachable from
/// the other half's factories.
///
/// # The three things that had to be true, each measured on its own binary
///
/// ```text
///   wave 5   LocaleResources.getBreakIteratorInfo / getBreakIteratorResources
///            answer from the image. `non_cldr_packages`: the family is not
///            one CLDR re-generated, and the blanket `cldr` mapping made
///            every candidate miss.
///   wave 6   setText(String) and preceding(int) -- the only two of the
///            seventeen that are CONCRETE on the abstract class -- step aside
///            for a receiver this VM did not fabricate. Until they did, the
///            chain built the RIGHT object and then wrote the text into slot 0
///            of an object whose slot 0 is `charCategoryTable`.
///   wave 6   the BREAKITER pin in `vm/src/vm/vm_exec.rs` is removed, so the
///            four static factories run their own bytecode.
/// ```
///
/// `apps/probes/L1BreakIterRealProbe` is the instrument, and it was built to
/// be measurable BEFORE any of this landed: its `P.*` rows reach
/// `BreakIteratorProviderImpl` directly, which the pin never covered. On the
/// control binary the chain already answered `sun.text.RuleBasedBreakIterator`
/// and every walk over it was `[0]`. On the wave-6 binary:
///
/// ```text
///   control  28 rows differ from HotSpot   (13 of them P.*, 15 F.*)
///   setText   16 rows differ               (0 P.*, all 16 the pinned F.*)
///   pin off    0 rows differ
/// ```
///
/// # What is NOT retired, and why the tempting row is absent
///
/// `java/text/BreakIterator` is the whole table. The natives stay registered
/// for synthetic-JDK mode, where there is no bytecode to prefer and the
/// fabricated carrier is the only BreakIterator there is -- retirement is
/// `--real-jdk`'s refusal and does not touch that mode.
///
/// The two `LocaleResources` readers wave 5 added are NOT retirable and must
/// not be swept in by a later widening of a prefix: they are the floor this
/// family now stands on, force-listed at both dispatch doors, and the family
/// returns to "Cannot load from null array" without them.
/// Lane 1 wave 6, 2026-09-11: the two locale-provider rows the vacuity
/// workload turned from non-answers into candidates, one method each.
///
/// Both were `reached == 0` in wave 5's bisection -- `+0` computed over an
/// empty set. `apps/probes/L1LocaleProviderWorkload` gave them a workload and
/// they came back with engagement and no divergence:
///
/// ```text
///   sun/util/resources/LocaleData.getBundle              +0   reached=21
///   .../JRELocaleProviderAdapter.getLocaleServiceProvider +0   reached=8
/// ```
///
/// Each class carries exactly ONE registration in the whole tree (grep for
/// the class name: one hit each, in `locale_resources.rs` and
/// `locale_bootstrap.rs`), so "retire the class" and "retire the method" are
/// the same act here and no half-retirement is possible.
///
/// THE TWO SIBLINGS THAT ARE NOT HERE ARE THE POINT. The same run measured
/// `LocaleResources` at +12 over 254 engagements and `CalendarDataUtility` at
/// +12 over 108, and the paragraph in `RETIRED_SHADOW_L1_ZI_TRIPLES` that
/// called all five "one blocker" was reasoning from `reached == 0`. Four of
/// them are candidates; two are measured blockers. Neither fact was visible
/// before there was a workload.
///
/// `sun/util/resources/Bundles` remains at `reached=0` even under the new
/// workload and is deliberately absent: a row nothing reaches is a row
/// nothing has measured.
pub(in crate::retired_shadow) static RETIRED_SHADOW_L1_LP_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "sun/util/locale/provider/JRELocaleProviderAdapter",
        "getLocaleServiceProvider",
        "(Ljava/lang/Class;)Ljava/util/spi/LocaleServiceProvider;",
    ),
    (
        "sun/util/resources/LocaleData",
        "getBundle",
        "(Ljava/lang/String;Ljava/util/Locale;)Ljava/util/ResourceBundle;",
    ),
];

/// Lane 1 wave 7, 2026-09-12: `java/util/Hashtable`, all 79, and what could
/// not go before was not the state model.
///
/// # What the page said, and what the binary says
///
/// Section 1 held this family as "state is a Rust side table, so retiring
/// hands real bytecode an empty map", and section 10 item 4 had it at `+10`
/// armed with the ten rows all VIEWS answering EMPTY. Both readings are
/// refuted by one run. On `cratonvm-l1w7-base-20260912`, `--real-jdk`,
/// UNARMED, read through `--add-opens java.base/java.util=ALL-UNNAMED`
/// (`apps/probes/L1MapStateDiag`, HotSpot 25.0.4+7 as the oracle):
///
/// ```text
///   Hashtable.table    [Ljava.util.Hashtable$Entry;  len=11 nonNull=3
///                      nodes java.util.Hashtable$Entry     SAME as HotSpot
///   Hashtable.count    3                                   SAME
///   Hashtable.modCount 3    threshold 8                    SAME
/// ```
///
/// The state IS real -- `H23-2`'s node-class move landed it -- so the armed
/// arm was measuring a DECODE, not a store. Every one of those ten rows was
/// `toArray()` over a view the image's own bytecode minted, and
/// `hs_backing_map` answered `None` for it: the source map is in `this$0`,
/// and this VM's own carriers keep it in a trailing slot the real class does
/// not have. `native-collections`' `hs_backing_map` and `hs_view_elements`
/// now read it by name, and the arm moves with them:
///
/// ```text
///   java/util/Hashtable armed, one class per process, whole probe tree
///     control  cratonvm-l1w7-base-20260912   7 probes worse, 30 rows
///     trial    cratonvm-l1w7-t3-20260912     0 probes worse
///       L1MapViewToArrayProbe (168 rows)     30 -> 0
///       L1EntrySetRouteProbe                 10 -> 0
/// ```
///
/// # Why the family retires WHOLE
///
/// `Hashtable.entrySet()` hands back a `Collections$SynchronizedSet` around
/// the inner carrier, so the accessor and the carrier are one unit: retire the
/// accessor alone and a real inner view meets natives that cannot read it;
/// retire the carrier alone and a VM-minted view meets real bytecode. The 79
/// are `Hashtable` (22), its three view classes (56) and `$Entry` (1).
///
/// # `Properties` is not in the blast radius, and that is measured
///
/// `Properties extends Hashtable`, and JDK 9 moved its storage into
/// `Properties.map`; it overrides the whole `Map` surface, so a dispatch door
/// asking about the DECLARING class reaches `Properties` and never
/// `Hashtable`. `PropertiesCloneProbe`, `SysPropsRealMapProbe` and the
/// `F.props.*` rows of `apps/probes/L1MapViewToArrayProbe` read identically on
/// the control and the trial.
pub(in crate::retired_shadow) static RETIRED_SHADOW_L1_HT_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/util/Hashtable", "<init>", "()V"),
    ("java/util/Hashtable", "<init>", "(I)V"),
    ("java/util/Hashtable", "<init>", "(IF)V"),
    ("java/util/Hashtable", "clear", "()V"),
    ("java/util/Hashtable", "clone", "()Ljava/lang/Object;"),
    (
        "java/util/Hashtable",
        "containsKey",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/Hashtable",
        "containsValue",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/Hashtable",
        "elements",
        "()Ljava/util/Enumeration;",
    ),
    ("java/util/Hashtable", "entrySet", "()Ljava/util/Set;"),
    (
        "java/util/Hashtable",
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    ("java/util/Hashtable", "isEmpty", "()Z"),
    ("java/util/Hashtable", "keySet", "()Ljava/util/Set;"),
    ("java/util/Hashtable", "keys", "()Ljava/util/Enumeration;"),
    (
        "java/util/Hashtable",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    ("java/util/Hashtable", "putAll", "(Ljava/util/Map;)V"),
    (
        "java/util/Hashtable",
        "remove",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/Hashtable",
        "remove",
        "(Ljava/lang/Object;Ljava/lang/Object;)Z",
    ),
    (
        "java/util/Hashtable",
        "replace",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/Hashtable",
        "replace",
        "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Z",
    ),
    ("java/util/Hashtable", "size", "()I"),
    ("java/util/Hashtable", "toString", "()Ljava/lang/String;"),
    ("java/util/Hashtable", "values", "()Ljava/util/Collection;"),
    (
        "java/util/Hashtable$Entry",
        "setValue",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/Hashtable$EntrySet",
        "add",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/Hashtable$EntrySet",
        "addAll",
        "(Ljava/util/Collection;)Z",
    ),
    ("java/util/Hashtable$EntrySet", "clear", "()V"),
    (
        "java/util/Hashtable$EntrySet",
        "contains",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/Hashtable$EntrySet",
        "containsAll",
        "(Ljava/util/Collection;)Z",
    ),
    (
        "java/util/Hashtable$EntrySet",
        "equals",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/Hashtable$EntrySet",
        "forEach",
        "(Ljava/util/function/Consumer;)V",
    ),
    ("java/util/Hashtable$EntrySet", "hashCode", "()I"),
    ("java/util/Hashtable$EntrySet", "isEmpty", "()Z"),
    (
        "java/util/Hashtable$EntrySet",
        "iterator",
        "()Ljava/util/Iterator;",
    ),
    (
        "java/util/Hashtable$EntrySet",
        "remove",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/Hashtable$EntrySet",
        "removeAll",
        "(Ljava/util/Collection;)Z",
    ),
    (
        "java/util/Hashtable$EntrySet",
        "removeIf",
        "(Ljava/util/function/Predicate;)Z",
    ),
    (
        "java/util/Hashtable$EntrySet",
        "retainAll",
        "(Ljava/util/Collection;)Z",
    ),
    ("java/util/Hashtable$EntrySet", "size", "()I"),
    (
        "java/util/Hashtable$EntrySet",
        "spliterator",
        "()Ljava/util/Spliterator;",
    ),
    (
        "java/util/Hashtable$EntrySet",
        "stream",
        "()Ljava/util/stream/Stream;",
    ),
    (
        "java/util/Hashtable$EntrySet",
        "toArray",
        "()[Ljava/lang/Object;",
    ),
    (
        "java/util/Hashtable$EntrySet",
        "toArray",
        "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;",
    ),
    (
        "java/util/Hashtable$EntrySet",
        "toArray",
        "([Ljava/lang/Object;)[Ljava/lang/Object;",
    ),
    (
        "java/util/Hashtable$EntrySet",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/util/Hashtable$KeySet", "add", "(Ljava/lang/Object;)Z"),
    (
        "java/util/Hashtable$KeySet",
        "addAll",
        "(Ljava/util/Collection;)Z",
    ),
    ("java/util/Hashtable$KeySet", "clear", "()V"),
    (
        "java/util/Hashtable$KeySet",
        "contains",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/Hashtable$KeySet",
        "containsAll",
        "(Ljava/util/Collection;)Z",
    ),
    (
        "java/util/Hashtable$KeySet",
        "equals",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/Hashtable$KeySet",
        "forEach",
        "(Ljava/util/function/Consumer;)V",
    ),
    ("java/util/Hashtable$KeySet", "hashCode", "()I"),
    ("java/util/Hashtable$KeySet", "isEmpty", "()Z"),
    (
        "java/util/Hashtable$KeySet",
        "iterator",
        "()Ljava/util/Iterator;",
    ),
    (
        "java/util/Hashtable$KeySet",
        "remove",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/Hashtable$KeySet",
        "removeAll",
        "(Ljava/util/Collection;)Z",
    ),
    (
        "java/util/Hashtable$KeySet",
        "removeIf",
        "(Ljava/util/function/Predicate;)Z",
    ),
    (
        "java/util/Hashtable$KeySet",
        "retainAll",
        "(Ljava/util/Collection;)Z",
    ),
    ("java/util/Hashtable$KeySet", "size", "()I"),
    (
        "java/util/Hashtable$KeySet",
        "spliterator",
        "()Ljava/util/Spliterator;",
    ),
    (
        "java/util/Hashtable$KeySet",
        "stream",
        "()Ljava/util/stream/Stream;",
    ),
    (
        "java/util/Hashtable$KeySet",
        "toArray",
        "()[Ljava/lang/Object;",
    ),
    (
        "java/util/Hashtable$KeySet",
        "toArray",
        "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;",
    ),
    (
        "java/util/Hashtable$KeySet",
        "toArray",
        "([Ljava/lang/Object;)[Ljava/lang/Object;",
    ),
    (
        "java/util/Hashtable$KeySet",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/util/Hashtable$ValueCollection", "clear", "()V"),
    (
        "java/util/Hashtable$ValueCollection",
        "contains",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/Hashtable$ValueCollection",
        "forEach",
        "(Ljava/util/function/Consumer;)V",
    ),
    ("java/util/Hashtable$ValueCollection", "isEmpty", "()Z"),
    (
        "java/util/Hashtable$ValueCollection",
        "iterator",
        "()Ljava/util/Iterator;",
    ),
    (
        "java/util/Hashtable$ValueCollection",
        "remove",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/Hashtable$ValueCollection",
        "removeIf",
        "(Ljava/util/function/Predicate;)Z",
    ),
    ("java/util/Hashtable$ValueCollection", "size", "()I"),
    (
        "java/util/Hashtable$ValueCollection",
        "spliterator",
        "()Ljava/util/Spliterator;",
    ),
    (
        "java/util/Hashtable$ValueCollection",
        "stream",
        "()Ljava/util/stream/Stream;",
    ),
    (
        "java/util/Hashtable$ValueCollection",
        "toArray",
        "()[Ljava/lang/Object;",
    ),
    (
        "java/util/Hashtable$ValueCollection",
        "toArray",
        "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;",
    ),
    (
        "java/util/Hashtable$ValueCollection",
        "toArray",
        "([Ljava/lang/Object;)[Ljava/lang/Object;",
    ),
    (
        "java/util/Hashtable$ValueCollection",
        "toString",
        "()Ljava/lang/String;",
    ),
];

/// L1 wave 8 — `java/util/TreeMap`, `java/util/TreeSet` and the family's view
/// and iterator carriers, 156 triples.
///
/// # What the hold said, and what was actually true
///
/// Section 1 of the lane page held this family on *"state is
/// `tm_array_table()`, a Rust side table"*, and `the_l1_families_measured_
/// load_bearing_are_still_held` said the same in four words: *"the real
/// `root`/`size`/`comparator` are never written"*. Wave 7 reflected them on an
/// unarmed control and all three matched HotSpot 25.0.4+7 —
/// `tm_publish_real_root` builds a real red-black tree of real
/// `java.util.TreeMap$Entry` nodes, `tm_set_slot` mirrors `size` by name, and
/// the comparator was always there. What was left of the hold was the
/// DIRECTION: that mirror was one-way, and its own doc said so.
///
/// # The two changes that made the family takeable
///
/// Both are in `native-collections`, and neither moves the store:
///
/// * `tm_pairs_from_real_root` — the READ half of that mirror. With the
///   natives gone, real bytecode maintains `root` and `tm_array_table` is
///   empty; a native that still has to decode a `TreeMap` (every `toArray`
///   over a view, every `new ArrayList<>(view)`) now reads the node graph.
/// * `ts_view_source`'s by-name arm and `collect_collection_elements_pinned`'s
///   `TreeMap$KeySet` arm — the view carriers. `TreeMap$KeySet` is in NEITHER
///   `MAP_VIEW_CARRIERS` nor `SET_VIEW_CARRIERS`, and `TreeMap$EntrySet` is in
///   the first but `vc_route`'s gate was the values predicate, so its
///   entry-shaped arm had never run.
///
/// And one correction that only the two-binary arm could find: the side table
/// gets SEEDED once on a read path and then rots, so `tm_collect_pairs` now
/// arbitrates between the two stores with the receiver's own `size` field
/// rather than preferring either. See its doc — `StaleViewAddAllProbe` is the
/// probe, and it was the only row in a 163-probe tree that moved.
///
/// # Measured
///
/// ```text
///   dial armed on `java/util/TreeMap` alone, apps/probes/L1MapViewToArrayProbe
///     control            38 rows differ       (unarmed: 0)
///     + decode fix       16 rows differ
///     + retirement        0 rows differ
/// ```
///
/// The 16 the dial could not clear are the dial's own: it declines at the
/// DISPATCH DOOR, so `native_ts_iterator` still ran and still minted a
/// `java/util/TreeMap$KeyIterator` whose own `hasNext` the same armed scope
/// then declined — real JDK bytecode over a carrier this VM laid out itself.
/// A retirement deletes the registration instead, so the iterator is the
/// image's and the question does not arise. Two instruments, and only the
/// second one can answer for a retirement.
///
/// # Acceptance
///
/// Two binaries from the same tree (`origin/dev` at `f99c2e748` merged in),
/// `cratonvm-l1w8-ctl2` and `cratonvm-l1w8-t7`, over the whole probe tree --
/// 163 measured, 9 the shared battery cannot compile:
///
/// ```text
///   --real-jdk     0 worse   2 better   0 line-count mismatches
///   DEFAULT mode   0 worse   1 better   0 line-count mismatches
///   regression-suite SUITE=all    136 / 136 on BOTH binaries
///   regression-suite SUITE=core    95 /  95 on BOTH binaries
/// ```
///
/// One "better" is attributable: `NullArgMsgProbe` -2, where
/// `TreeSet.addAll(null)` now carries the helpful NPE text real bytecode
/// builds instead of the native's bare `null`. The other two are not claimed
/// -- `L5FjDouble` is a ForkJoin scheduling row, and `ConcurrentStressSweep`
/// is the probe wave 7 measured moving AGAINST ITSELF, 0/4/2/4 over four
/// control-versus-control runs.
///
/// A control arm was run first, same script, the same binary on both sides:
/// 163 probes, 0 worse, 0 better. The noise floor for this battery on this
/// host is zero, which is what lets the two rows above be read at all.
///
/// The nine probes the battery skips for want of `--add-exports` were run by
/// hand against both binaries: all five that compile read base 0 / trial 0.
///
///
/// # Re-taken on the landing tree
///
/// `origin/dev` moved 35 commits mid-wave, and one of them was a sibling
/// lane's fix to `TreeMap`'s declared reference fields (`cached_tm_view`, the
/// same function family). So the whole set was re-run against a control built
/// from `0a805f3fc` itself:
///
/// ```text
///   --real-jdk     164 probes   0 worse   1 better (NullArgMsgProbe -2)
///   DEFAULT        164 probes   2 worse   1 better  <- both "worse" are flakes
///   regression-suite SUITE=all   trial 136 / 136, control 135 / 136
///   regression-suite SUITE=core   95 /  95 on both
/// ```
///
/// The two default-mode rows are `ChmShadowSweep` (+12) and
/// `ConcurrentStressSweep` (+2), and they are the same `ConcurrentHashMap`
/// race the strict corpus prints as `T19_H6_CAS_DIAG`. Measured rather than
/// asserted: 20 INTERLEAVED pairs of `ChmShadowSweep` in default mode give
/// **control 5/20 non-zero, trial 3/20** — the probe loses one of 800
/// concurrent writes about a fifth of the time on either binary, and the
/// battery's single sample happened to catch the control clean. The control's
/// own `SUITE=all` lost `RMapGcStress` in the same way while the trial passed
/// 136/136.
/// # Scope
///
/// 156 rows: every `owns_slot && kind == "bridge"` registration under the two
/// prefixes that is bucket A or B **on all three supported images** —
/// jdk-17.0.20.1+1, jdk-21.0.12+8 and jdk-25.0.4+7, censused separately rather
/// than assumed from the newest. Seven rows are refused by all three and stay
/// out: `TreeMap.iterator()`, which no image declares, and six
/// `TreeMap$KeySet` members (`<init>` ×3, `clone`, `readObject`, `writeObject`)
/// that exist only because `register_tree_set_natives` mirrors `TreeSet`'s
/// surface onto the carrier.
///
/// The family's three iterator carriers are in the table WITH their producers,
/// which is the condition
/// `a-carrier-minted-for-other-families-blocks-its-own-retirement` names:
/// `TreeMap$KeyIterator` is minted for `java/util/TreeSet` and
/// `TreeMap$KeySet`, `$ValueIterator` for `TreeMap$Values`, `$EntryIterator`
/// for `TreeMap$EntrySet` — and all five of those classes are retired here.
/// Nothing outside the family mints one.
pub(in crate::retired_shadow) static RETIRED_SHADOW_L1_TM_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/util/TreeMap", "<init>", "()V"),
    ("java/util/TreeMap", "<init>", "(Ljava/util/Comparator;)V"),
    ("java/util/TreeMap", "<init>", "(Ljava/util/Map;)V"),
    ("java/util/TreeMap", "<init>", "(Ljava/util/SortedMap;)V"),
    (
        "java/util/TreeMap",
        "ceilingEntry",
        "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
    ),
    (
        "java/util/TreeMap",
        "ceilingKey",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    ("java/util/TreeMap", "clear", "()V"),
    (
        "java/util/TreeMap",
        "comparator",
        "()Ljava/util/Comparator;",
    ),
    (
        "java/util/TreeMap",
        "compute",
        "(Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
    ),
    (
        "java/util/TreeMap",
        "computeIfAbsent",
        "(Ljava/lang/Object;Ljava/util/function/Function;)Ljava/lang/Object;",
    ),
    (
        "java/util/TreeMap",
        "computeIfPresent",
        "(Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
    ),
    ("java/util/TreeMap", "containsKey", "(Ljava/lang/Object;)Z"),
    (
        "java/util/TreeMap",
        "containsValue",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/TreeMap",
        "descendingKeySet",
        "()Ljava/util/NavigableSet;",
    ),
    (
        "java/util/TreeMap",
        "descendingMap",
        "()Ljava/util/NavigableMap;",
    ),
    ("java/util/TreeMap", "entrySet", "()Ljava/util/Set;"),
    ("java/util/TreeMap", "firstEntry", "()Ljava/util/Map$Entry;"),
    ("java/util/TreeMap", "firstKey", "()Ljava/lang/Object;"),
    (
        "java/util/TreeMap",
        "floorEntry",
        "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
    ),
    (
        "java/util/TreeMap",
        "floorKey",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/TreeMap",
        "forEach",
        "(Ljava/util/function/BiConsumer;)V",
    ),
    (
        "java/util/TreeMap",
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/TreeMap",
        "getOrDefault",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/TreeMap",
        "headMap",
        "(Ljava/lang/Object;)Ljava/util/SortedMap;",
    ),
    (
        "java/util/TreeMap",
        "headMap",
        "(Ljava/lang/Object;Z)Ljava/util/NavigableMap;",
    ),
    (
        "java/util/TreeMap",
        "higherEntry",
        "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
    ),
    (
        "java/util/TreeMap",
        "higherKey",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    ("java/util/TreeMap", "isEmpty", "()Z"),
    ("java/util/TreeMap", "keySet", "()Ljava/util/Set;"),
    ("java/util/TreeMap", "lastEntry", "()Ljava/util/Map$Entry;"),
    ("java/util/TreeMap", "lastKey", "()Ljava/lang/Object;"),
    (
        "java/util/TreeMap",
        "lowerEntry",
        "(Ljava/lang/Object;)Ljava/util/Map$Entry;",
    ),
    (
        "java/util/TreeMap",
        "lowerKey",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/TreeMap",
        "merge",
        "(Ljava/lang/Object;Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
    ),
    (
        "java/util/TreeMap",
        "navigableKeySet",
        "()Ljava/util/NavigableSet;",
    ),
    (
        "java/util/TreeMap",
        "pollFirstEntry",
        "()Ljava/util/Map$Entry;",
    ),
    (
        "java/util/TreeMap",
        "pollLastEntry",
        "()Ljava/util/Map$Entry;",
    ),
    (
        "java/util/TreeMap",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    ("java/util/TreeMap", "putAll", "(Ljava/util/Map;)V"),
    (
        "java/util/TreeMap",
        "putIfAbsent",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/TreeMap",
        "remove",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/TreeMap",
        "remove",
        "(Ljava/lang/Object;Ljava/lang/Object;)Z",
    ),
    (
        "java/util/TreeMap",
        "replace",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/TreeMap",
        "replace",
        "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Z",
    ),
    (
        "java/util/TreeMap",
        "replaceAll",
        "(Ljava/util/function/BiFunction;)V",
    ),
    ("java/util/TreeMap", "size", "()I"),
    (
        "java/util/TreeMap",
        "subMap",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/SortedMap;",
    ),
    (
        "java/util/TreeMap",
        "subMap",
        "(Ljava/lang/Object;ZLjava/lang/Object;Z)Ljava/util/NavigableMap;",
    ),
    (
        "java/util/TreeMap",
        "tailMap",
        "(Ljava/lang/Object;)Ljava/util/SortedMap;",
    ),
    (
        "java/util/TreeMap",
        "tailMap",
        "(Ljava/lang/Object;Z)Ljava/util/NavigableMap;",
    ),
    ("java/util/TreeMap", "toString", "()Ljava/lang/String;"),
    ("java/util/TreeMap", "values", "()Ljava/util/Collection;"),
    (
        "java/util/TreeMap$Entry",
        "setValue",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    ("java/util/TreeMap$EntryIterator", "hasNext", "()Z"),
    (
        "java/util/TreeMap$EntryIterator",
        "next",
        "()Ljava/lang/Object;",
    ),
    ("java/util/TreeMap$EntryIterator", "remove", "()V"),
    ("java/util/TreeMap$EntrySet", "clear", "()V"),
    (
        "java/util/TreeMap$EntrySet",
        "contains",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/TreeMap$EntrySet",
        "forEach",
        "(Ljava/util/function/Consumer;)V",
    ),
    ("java/util/TreeMap$EntrySet", "isEmpty", "()Z"),
    (
        "java/util/TreeMap$EntrySet",
        "iterator",
        "()Ljava/util/Iterator;",
    ),
    (
        "java/util/TreeMap$EntrySet",
        "remove",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/TreeMap$EntrySet",
        "removeIf",
        "(Ljava/util/function/Predicate;)Z",
    ),
    ("java/util/TreeMap$EntrySet", "size", "()I"),
    (
        "java/util/TreeMap$EntrySet",
        "spliterator",
        "()Ljava/util/Spliterator;",
    ),
    (
        "java/util/TreeMap$EntrySet",
        "stream",
        "()Ljava/util/stream/Stream;",
    ),
    (
        "java/util/TreeMap$EntrySet",
        "toArray",
        "()[Ljava/lang/Object;",
    ),
    (
        "java/util/TreeMap$EntrySet",
        "toArray",
        "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;",
    ),
    (
        "java/util/TreeMap$EntrySet",
        "toArray",
        "([Ljava/lang/Object;)[Ljava/lang/Object;",
    ),
    (
        "java/util/TreeMap$EntrySet",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/util/TreeMap$KeyIterator", "hasNext", "()Z"),
    (
        "java/util/TreeMap$KeyIterator",
        "next",
        "()Ljava/lang/Object;",
    ),
    ("java/util/TreeMap$KeyIterator", "remove", "()V"),
    ("java/util/TreeMap$KeySet", "add", "(Ljava/lang/Object;)Z"),
    (
        "java/util/TreeMap$KeySet",
        "addAll",
        "(Ljava/util/Collection;)Z",
    ),
    (
        "java/util/TreeMap$KeySet",
        "ceiling",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    ("java/util/TreeMap$KeySet", "clear", "()V"),
    (
        "java/util/TreeMap$KeySet",
        "comparator",
        "()Ljava/util/Comparator;",
    ),
    (
        "java/util/TreeMap$KeySet",
        "contains",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/TreeMap$KeySet",
        "descendingIterator",
        "()Ljava/util/Iterator;",
    ),
    (
        "java/util/TreeMap$KeySet",
        "descendingSet",
        "()Ljava/util/NavigableSet;",
    ),
    ("java/util/TreeMap$KeySet", "first", "()Ljava/lang/Object;"),
    (
        "java/util/TreeMap$KeySet",
        "floor",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/TreeMap$KeySet",
        "forEach",
        "(Ljava/util/function/Consumer;)V",
    ),
    (
        "java/util/TreeMap$KeySet",
        "headSet",
        "(Ljava/lang/Object;)Ljava/util/SortedSet;",
    ),
    (
        "java/util/TreeMap$KeySet",
        "headSet",
        "(Ljava/lang/Object;Z)Ljava/util/NavigableSet;",
    ),
    (
        "java/util/TreeMap$KeySet",
        "higher",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    ("java/util/TreeMap$KeySet", "isEmpty", "()Z"),
    (
        "java/util/TreeMap$KeySet",
        "iterator",
        "()Ljava/util/Iterator;",
    ),
    ("java/util/TreeMap$KeySet", "last", "()Ljava/lang/Object;"),
    (
        "java/util/TreeMap$KeySet",
        "lower",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/TreeMap$KeySet",
        "pollFirst",
        "()Ljava/lang/Object;",
    ),
    (
        "java/util/TreeMap$KeySet",
        "pollLast",
        "()Ljava/lang/Object;",
    ),
    (
        "java/util/TreeMap$KeySet",
        "remove",
        "(Ljava/lang/Object;)Z",
    ),
    ("java/util/TreeMap$KeySet", "size", "()I"),
    (
        "java/util/TreeMap$KeySet",
        "spliterator",
        "()Ljava/util/Spliterator;",
    ),
    (
        "java/util/TreeMap$KeySet",
        "stream",
        "()Ljava/util/stream/Stream;",
    ),
    (
        "java/util/TreeMap$KeySet",
        "subSet",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/SortedSet;",
    ),
    (
        "java/util/TreeMap$KeySet",
        "subSet",
        "(Ljava/lang/Object;ZLjava/lang/Object;Z)Ljava/util/NavigableSet;",
    ),
    (
        "java/util/TreeMap$KeySet",
        "tailSet",
        "(Ljava/lang/Object;)Ljava/util/SortedSet;",
    ),
    (
        "java/util/TreeMap$KeySet",
        "tailSet",
        "(Ljava/lang/Object;Z)Ljava/util/NavigableSet;",
    ),
    (
        "java/util/TreeMap$KeySet",
        "toArray",
        "()[Ljava/lang/Object;",
    ),
    (
        "java/util/TreeMap$KeySet",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/util/TreeMap$ValueIterator", "hasNext", "()Z"),
    (
        "java/util/TreeMap$ValueIterator",
        "next",
        "()Ljava/lang/Object;",
    ),
    ("java/util/TreeMap$ValueIterator", "remove", "()V"),
    ("java/util/TreeMap$Values", "clear", "()V"),
    (
        "java/util/TreeMap$Values",
        "contains",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/TreeMap$Values",
        "forEach",
        "(Ljava/util/function/Consumer;)V",
    ),
    ("java/util/TreeMap$Values", "isEmpty", "()Z"),
    (
        "java/util/TreeMap$Values",
        "iterator",
        "()Ljava/util/Iterator;",
    ),
    (
        "java/util/TreeMap$Values",
        "remove",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/util/TreeMap$Values",
        "removeIf",
        "(Ljava/util/function/Predicate;)Z",
    ),
    ("java/util/TreeMap$Values", "size", "()I"),
    (
        "java/util/TreeMap$Values",
        "spliterator",
        "()Ljava/util/Spliterator;",
    ),
    (
        "java/util/TreeMap$Values",
        "stream",
        "()Ljava/util/stream/Stream;",
    ),
    (
        "java/util/TreeMap$Values",
        "toArray",
        "()[Ljava/lang/Object;",
    ),
    (
        "java/util/TreeMap$Values",
        "toArray",
        "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;",
    ),
    (
        "java/util/TreeMap$Values",
        "toArray",
        "([Ljava/lang/Object;)[Ljava/lang/Object;",
    ),
    (
        "java/util/TreeMap$Values",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/util/TreeSet", "<init>", "()V"),
    ("java/util/TreeSet", "<init>", "(Ljava/util/Collection;)V"),
    ("java/util/TreeSet", "<init>", "(Ljava/util/Comparator;)V"),
    ("java/util/TreeSet", "add", "(Ljava/lang/Object;)Z"),
    ("java/util/TreeSet", "addAll", "(Ljava/util/Collection;)Z"),
    (
        "java/util/TreeSet",
        "ceiling",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    ("java/util/TreeSet", "clear", "()V"),
    ("java/util/TreeSet", "clone", "()Ljava/lang/Object;"),
    (
        "java/util/TreeSet",
        "comparator",
        "()Ljava/util/Comparator;",
    ),
    ("java/util/TreeSet", "contains", "(Ljava/lang/Object;)Z"),
    (
        "java/util/TreeSet",
        "descendingIterator",
        "()Ljava/util/Iterator;",
    ),
    (
        "java/util/TreeSet",
        "descendingSet",
        "()Ljava/util/NavigableSet;",
    ),
    ("java/util/TreeSet", "first", "()Ljava/lang/Object;"),
    (
        "java/util/TreeSet",
        "floor",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/util/TreeSet",
        "forEach",
        "(Ljava/util/function/Consumer;)V",
    ),
    (
        "java/util/TreeSet",
        "headSet",
        "(Ljava/lang/Object;)Ljava/util/SortedSet;",
    ),
    (
        "java/util/TreeSet",
        "headSet",
        "(Ljava/lang/Object;Z)Ljava/util/NavigableSet;",
    ),
    (
        "java/util/TreeSet",
        "higher",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    ("java/util/TreeSet", "isEmpty", "()Z"),
    ("java/util/TreeSet", "iterator", "()Ljava/util/Iterator;"),
    ("java/util/TreeSet", "last", "()Ljava/lang/Object;"),
    (
        "java/util/TreeSet",
        "lower",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    ("java/util/TreeSet", "pollFirst", "()Ljava/lang/Object;"),
    ("java/util/TreeSet", "pollLast", "()Ljava/lang/Object;"),
    (
        "java/util/TreeSet",
        "readObject",
        "(Ljava/io/ObjectInputStream;)V",
    ),
    ("java/util/TreeSet", "remove", "(Ljava/lang/Object;)Z"),
    ("java/util/TreeSet", "size", "()I"),
    (
        "java/util/TreeSet",
        "spliterator",
        "()Ljava/util/Spliterator;",
    ),
    ("java/util/TreeSet", "stream", "()Ljava/util/stream/Stream;"),
    (
        "java/util/TreeSet",
        "subSet",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/SortedSet;",
    ),
    (
        "java/util/TreeSet",
        "subSet",
        "(Ljava/lang/Object;ZLjava/lang/Object;Z)Ljava/util/NavigableSet;",
    ),
    (
        "java/util/TreeSet",
        "tailSet",
        "(Ljava/lang/Object;)Ljava/util/SortedSet;",
    ),
    (
        "java/util/TreeSet",
        "tailSet",
        "(Ljava/lang/Object;Z)Ljava/util/NavigableSet;",
    ),
    ("java/util/TreeSet", "toArray", "()[Ljava/lang/Object;"),
    ("java/util/TreeSet", "toString", "()Ljava/lang/String;"),
    (
        "java/util/TreeSet",
        "writeObject",
        "(Ljava/io/ObjectOutputStream;)V",
    ),
];

pub(in crate::retired_shadow) static RETIRED_SHADOW_L1_BI_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/text/BreakIterator", "current", "()I"),
    ("java/text/BreakIterator", "first", "()I"),
    ("java/text/BreakIterator", "following", "(I)I"),
    (
        "java/text/BreakIterator",
        "getCharacterInstance",
        "()Ljava/text/BreakIterator;",
    ),
    (
        "java/text/BreakIterator",
        "getCharacterInstance",
        "(Ljava/util/Locale;)Ljava/text/BreakIterator;",
    ),
    (
        "java/text/BreakIterator",
        "getLineInstance",
        "()Ljava/text/BreakIterator;",
    ),
    (
        "java/text/BreakIterator",
        "getLineInstance",
        "(Ljava/util/Locale;)Ljava/text/BreakIterator;",
    ),
    (
        "java/text/BreakIterator",
        "getSentenceInstance",
        "()Ljava/text/BreakIterator;",
    ),
    (
        "java/text/BreakIterator",
        "getSentenceInstance",
        "(Ljava/util/Locale;)Ljava/text/BreakIterator;",
    ),
    (
        "java/text/BreakIterator",
        "getText",
        "()Ljava/text/CharacterIterator;",
    ),
    (
        "java/text/BreakIterator",
        "getWordInstance",
        "()Ljava/text/BreakIterator;",
    ),
    (
        "java/text/BreakIterator",
        "getWordInstance",
        "(Ljava/util/Locale;)Ljava/text/BreakIterator;",
    ),
    ("java/text/BreakIterator", "last", "()I"),
    ("java/text/BreakIterator", "next", "()I"),
    ("java/text/BreakIterator", "preceding", "(I)I"),
    ("java/text/BreakIterator", "previous", "()I"),
    (
        "java/text/BreakIterator",
        "setText",
        "(Ljava/lang/String;)V",
    ),
];

/// Lane 4 wave 1, 2026-09-11: 140 rows over 10 classes of `java/io/` and
/// `java/nio/`.
///
/// A SEVENTH table rather than rows merged into a sibling, for the reason the
/// second and third give for existing: these were adjudicated by a different
/// METHOD, and the method is the part worth seeing at a glance.
///
/// # The method: a per-family dial sweep, then the SAME families armed together
///
/// `l4famsweep.sh` arms `CRATONVM_ENFORCE_NATIVE_SHADOW` on one
/// receiver family at a time and diffs thirteen lane-4 probes against a real
/// HotSpot 25 on the same host. On 2026-09-10, with the `FileInputStream.skip`
/// residual fixed, sixteen of its eighteen families scored **0**:
///
/// ```text
///   java/io/PrintStream  File  FileInputStream  FileOutputStream
///   java/io/ByteArrayInputStream  ByteArrayOutputStream  DataInputStream
///   java/io/DataOutputStream  BufferedReader  BufferedWriter
///   java/io/FilterOutputStream   java/nio/ByteBuffer   java/nio/CharBuffer
///   java/nio/file/spi/FileSystemProvider   java/nio/file/attribute/
///   java/nio/channels/FileChannel                                    DIFF 0
///   java/nio/file/Files                                              DIFF 4
///   java/nio/file/Path                                             DIFF 100
/// ```
///
/// **Sixteen per-family greens are not one green for the wave.** Phase 2 armed
/// 270 classes one at a time, called 236 retire-safe, and arming those 236
/// together failed 54 of 118 corpus vectors. So the families were re-armed as
/// ONE scope, which is what a wave actually does, and measured against an
/// unarmed control on the same binary:
///
/// ```text
///   control (dial off)                 0 diffs over 4416 rows, 0 short
///   the 13 wave-1 families armed       0 diffs over 4416 rows, 0 short
///   ... plus java/io/PrintStream       0 diffs over 4416 rows, 0 short
/// ```
///
/// # The dial was ASKED, and this is the number that says so
///
/// A zero from an instrument that never ran reads exactly like a zero from one
/// that did. Under the wave-1 scope the dial's own counters report **180 268
/// dispatches reached and 176 112 YIELDED** across the thirteen probes -- that
/// many native calls declined and served by real JDK bytecode instead, for
/// 4 416 rows that stayed byte-identical to HotSpot. `L4TypedBufferSweep`
/// alone yields 27 849 times and moves no row.
///
/// # The funnel
///
/// Step 1 of the lane's increment loop, unioned over the whole instrument
/// rather than one probe: a registry dump per probe (13 dumps, schema 5,
/// `--explain-jdk-only` so the image columns are populated), and a row is a
/// candidate only if it **owns its slot**, is kind `Bridge`, its image
/// declaring method carries `Code` (bucket A or B), and it was **invoked at
/// least once** in one of those runs.
///
/// ```text
///   bucket C / D / E / F (no bytecode to yield to)      867  not candidates
///   not a Bridge                                         17
///   invocations 0 in all 13 runs                        820  unmeasured, not safe
///   outside a DIFF-0 family                             226  Files, Path, sun/nio/ch, …
///   CANDIDATES                                          191
///   backed out by the BUILD (see below)                  -51
///   RETIRED                                             140
/// ```
///
/// `invocations 0` is the largest exclusion and it is deliberately NOT read as
/// "unreachable": schema 5's `invocations_complete` bit exists because the
/// column is a lower bound. Those 820 rows are unmeasured by this instrument,
/// which is a reason not to retire them here, not a verdict about them.
///
/// # What the build found that the dial could not, and why the dial cannot
///
/// The screen above said 0 for all thirteen probes with the wave armed. Built,
/// the same thirteen probes moved **eight rows in strict mode** — and none in
/// compatible mode, which is the half that identifies the mechanism:
///
/// ```text
///   L4TailSweep2        3 rows   Files.createLink -> UnsupportedOperationException
///   L4TypedBufferSweep  3 rows   CharBuffer subSequence/toString, wrong WINDOW
///   TailFamilySweep     2 rows   the same two, through the asCharBuffer view
///
/// (`diff` counts both sides, so that is 16 differing LINES. The count that
/// matters is eight rows, in two families.)
/// ```
///
/// **The dial is not a faithful model of a retirement, and this is the shape of
/// the gap.** The dial declines a native at DISPATCH, and its decline is
/// conditional: when the receiver's own class has no concrete body to yield to
/// (`dispatch_has_code == false`) it answers no and the native runs anyway —
/// that is the `declined_no_bytecode` column, 4 156 of 180 268 here. A
/// retirement has no such fallback: the registration is simply not there, and
/// the call lands wherever real dispatch takes it. Every row the dial scored 0
/// on *because it declined to decline* is therefore unmeasured by it.
///
/// `java/nio/file/spi/FileSystemProvider` is exactly that row. This VM hands
/// out a fabricated provider stamped with the ABSTRACT class, so `createLink`
/// has no concrete body, so the dial ran the native and read 0. Retired, the
/// call reaches the abstract declaration and answers
/// `UnsupportedOperationException` for all three rows — the
/// fabricate-an-abstract-class trade, arrived at from the registration side.
/// All four of its rows are out.
///
/// `java/nio/CharBuffer` is the second, and it is §4's named failure mode
/// rather than a throw: `subSequence(1,3)` answered `cd` where HotSpot answers
/// `bc`, and `toString()` after `position(2)` answered the empty string. The
/// real `CharBuffer` bodies derive their window from `position()`/`limit()`,
/// and this VM's carrier does not keep those where the real accessors read
/// them, so retiring the accessors hands the real bodies the wrong window. It
/// produces correct-LOOKING output with the wrong characters, which is the one
/// outcome the lane page says to design the probe for. All seventeen of its
/// rows are out. `java/nio/ByteBuffer`'s thirty-four stay: the same accessors,
/// the same probe, zero rows moved — so this is a CharBuffer carrier defect and
/// not a buffer-wide one.
///
/// The four `ByteBufferAsCharBuffer{B,L,RB,RL}.order()` rows stay too. They were
/// suspected with CharBuffer and cleared by the re-measurement: `order()` is a
/// constant, and with CharBuffer's accessors restored those views are at 0.
///
/// # The corpus found three more, and the probe tree could not have
///
/// The thirteen probes scored 0 in BOTH modes on the built binary -- 4 416 rows,
/// binary against binary. The `--real-jdk` corpus went **132/132 -> 129/132**,
/// reproducibly, on the same binary:
///
/// ```text
///   RFileTimes       every timestamp reads 1970-01-01T00:00:00Z
///   RJdkSecurity     a property-named truststore IGNORED: 122 anchors for 1,
///                    and a certificate HotSpot REJECTS is ACCEPTED
///   RSslLiveSession  fails at client.responseCode = 200
/// ```
///
/// `RFileTimes` has one cause and it is the SETTER, not the getter:
/// `Files.setLastModifiedTime` reads `FileTime.toMillis()` off a fabricated
/// carrier, gets 0, and stamps the file at the epoch -- so all four read-back
/// rows follow from one write. Dial-armed attribution names
/// `java/nio/file/attribute/` for it and `java/io/ByteArrayInputStream` for
/// `RSslLiveSession`; `java/io/File` armed alone is clean, so `File.lastModified`
/// reading 1970 was a symptom of the same write and not a second defect.
///
/// `RJdkSecurity` reproduces under NO single family and not under all thirteen
/// armed together -- the dial's blind spot described above, and the dial was
/// never going to name it. What did was an eight-line probe,
/// `L4AbsPath.java`, printing path SHAPES rather than paths:
///
/// ```text
///   temp.getPath          abs=true len=32 slashes=2      (correct)
///   temp.isAbsolute       false                          (HotSpot: true)
///   temp.getAbsolutePath  abs=true len=45 slashes=5      (the cwd, prepended)
/// ```
///
/// Every `java.io.File` this VM builds kept its path in slot 0 and wrote
/// nothing else, so real `File` bytecode read `prefixLength = 0` and called
/// every path relative. Fixed in the commit before this table, in
/// [`crate::file_layout`], at all six producing call sites across two crates.
/// **That fix is why the six `prefixLength`-dependent `File` rows below --
/// `isAbsolute`, `getAbsolutePath`, `getAbsoluteFile`, `getCanonicalPath`,
/// `getCanonicalFile`, `toURI` -- are retired here rather than carved out.**
///
/// Five fabricated carriers in this lane have now been found with their real
/// fields empty: `Path`, `CharBuffer`, `FileTime`, `FileSystemProvider` and
/// `File`. The carrier is the blocker, not the retirement.
///
/// The whole `--real-jdk` corpus, armed on the trimmed scope, is back at its
/// unarmed baseline. **That screen belongs before the build, not after it**, and
/// its absence is what cost this wave five of its seven builds.
///
/// # What is held back, and why
///
///  * **`java/io/PrintStream`.** It scores 0 armed, alone
///    and in the wave, and it is still not here: `System.out` and `System.err`
///    are how every lane reads its probes, so a regression there reads as all
///    nine lanes failing at once. The lane page requires it in its own wave
///    with its own commit naming the blast radius. The measurement above is
///    that wave's evidence, taken early.
///  * **The five families the builds backed out (51 rows).**
///    `java/nio/CharBuffer` (17) and `java/nio/file/spi/FileSystemProvider` (4)
///    from the probe tree; `java/nio/file/attribute/` (12) and
///    `java/io/ByteArrayInputStream` (9) from the corpus, attributed by dial
///    sweep; and the file-handle group (9) — `java/io/FileOutputStream`,
///    `FileCleanable`, `FileDescriptor` and the abstract-receiver
///    `java/nio/channels/FileChannel` — as an un-attributed GROUP, because
///    `RJdkSecurity` reproduces under no dial scope at all. Every one of them is
///    blocked on a CARRIER defect rather than on the retirement: a fabricated
///    receiver whose real fields this VM never writes.
///  * **`java/nio/file/Files` (DIFF 4) and `java/nio/file/Path` (DIFF 100).**
///    Above the floor, so not candidates. `Path`'s 100 are one defect: the
///    carrier is stamped with the INTERFACE, so `toString()` lands on
///    `Object.toString()`. See `crate::path_layout`.
///  * **`sun/nio/ch/`.** The 2026-08-19 package verdict stands; this wave does
///    not try to beat it.
///  * **`native-io/src/concrete_receiver.rs:185`**, a cross-lane registrar of
///    191 rows over 21 classes that lane T owns whole. The funnel filters it by
///    call site and it removed NOTHING here -- its classes are `sun/nio/ch`,
///    already outside the wave -- so the filter is a guard for later waves
///    rather than a thing that fired.
///  * **`jdk/internal/foreign`.** Nothing under it reached the candidate set,
///    so its prefix is not admitted either.
///
/// The wave's only FFM-adjacent rows went out with CharBuffer:
/// `session()Ljdk/internal/foreign/MemorySessionImpl;` and `checkSession()V`
/// were on that class. Nothing retired here touches
/// `jdk/internal/foreign`, whose carrier is this VM's own allocation shape
/// rather than the JDK's
/// (`docs/feature-designs/native-dispatch-architecture.md`).
///
/// Measured on **linux/x86_64 against JDK 25**. `java/io/File`'s separators,
/// absolute-path rules and permission methods differ by OS, and the two shell
/// gates in this campaign are keyed `25/linux` and refuse on Windows, so that
/// is the platform this table is measured on and the only one.
pub(in crate::retired_shadow) static RETIRED_SHADOW_L4_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/io/ByteArrayOutputStream", "<init>", "()V"),
    ("java/io/ByteArrayOutputStream", "<init>", "(I)V"),
    ("java/io/ByteArrayOutputStream", "close", "()V"),
    ("java/io/ByteArrayOutputStream", "flush", "()V"),
    ("java/io/ByteArrayOutputStream", "reset", "()V"),
    ("java/io/ByteArrayOutputStream", "size", "()I"),
    ("java/io/ByteArrayOutputStream", "toByteArray", "()[B"),
    (
        "java/io/ByteArrayOutputStream",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "java/io/ByteArrayOutputStream",
        "toString",
        "(Ljava/lang/String;)Ljava/lang/String;",
    ),
    (
        "java/io/ByteArrayOutputStream",
        "toString",
        "(Ljava/nio/charset/Charset;)Ljava/lang/String;",
    ),
    ("java/io/ByteArrayOutputStream", "write", "(I)V"),
    ("java/io/ByteArrayOutputStream", "write", "([B)V"),
    ("java/io/ByteArrayOutputStream", "write", "([BII)V"),
    ("java/io/DataInputStream", "available", "()I"),
    ("java/io/DataInputStream", "close", "()V"),
    ("java/io/DataInputStream", "read", "()I"),
    ("java/io/DataInputStream", "read", "([BII)I"),
    ("java/io/DataInputStream", "readBoolean", "()Z"),
    ("java/io/DataInputStream", "readByte", "()B"),
    ("java/io/DataInputStream", "readChar", "()C"),
    ("java/io/DataInputStream", "readDouble", "()D"),
    ("java/io/DataInputStream", "readFloat", "()F"),
    ("java/io/DataInputStream", "readFully", "([B)V"),
    ("java/io/DataInputStream", "readFully", "([BII)V"),
    ("java/io/DataInputStream", "readInt", "()I"),
    ("java/io/DataInputStream", "readLong", "()J"),
    ("java/io/DataInputStream", "readShort", "()S"),
    ("java/io/DataInputStream", "readUTF", "()Ljava/lang/String;"),
    ("java/io/DataInputStream", "readUnsignedByte", "()I"),
    ("java/io/DataInputStream", "readUnsignedShort", "()I"),
    ("java/io/DataInputStream", "skipBytes", "(I)I"),
    (
        "java/io/DataOutputStream",
        "<init>",
        "(Ljava/io/OutputStream;)V",
    ),
    ("java/io/DataOutputStream", "close", "()V"),
    ("java/io/DataOutputStream", "flush", "()V"),
    ("java/io/DataOutputStream", "size", "()I"),
    ("java/io/DataOutputStream", "write", "(I)V"),
    ("java/io/DataOutputStream", "write", "([BII)V"),
    ("java/io/DataOutputStream", "writeBoolean", "(Z)V"),
    ("java/io/DataOutputStream", "writeByte", "(I)V"),
    ("java/io/DataOutputStream", "writeChar", "(I)V"),
    ("java/io/DataOutputStream", "writeDouble", "(D)V"),
    ("java/io/DataOutputStream", "writeFloat", "(F)V"),
    ("java/io/DataOutputStream", "writeInt", "(I)V"),
    ("java/io/DataOutputStream", "writeLong", "(J)V"),
    ("java/io/DataOutputStream", "writeShort", "(I)V"),
    (
        "java/io/DataOutputStream",
        "writeUTF",
        "(Ljava/lang/String;)V",
    ),
    (
        "java/io/File",
        "<init>",
        "(Ljava/io/File;Ljava/lang/String;)V",
    ),
    ("java/io/File", "<init>", "(Ljava/lang/String;)V"),
    (
        "java/io/File",
        "<init>",
        "(Ljava/lang/String;Ljava/lang/String;)V",
    ),
    ("java/io/File", "<init>", "(Ljava/net/URI;)V"),
    ("java/io/File", "canExecute", "()Z"),
    ("java/io/File", "canRead", "()Z"),
    ("java/io/File", "canWrite", "()Z"),
    ("java/io/File", "compareTo", "(Ljava/io/File;)I"),
    ("java/io/File", "createNewFile", "()Z"),
    (
        "java/io/File",
        "createTempFile",
        "(Ljava/lang/String;Ljava/lang/String;Ljava/io/File;)Ljava/io/File;",
    ),
    ("java/io/File", "delete", "()Z"),
    ("java/io/File", "equals", "(Ljava/lang/Object;)Z"),
    ("java/io/File", "exists", "()Z"),
    ("java/io/File", "getAbsoluteFile", "()Ljava/io/File;"),
    ("java/io/File", "getAbsolutePath", "()Ljava/lang/String;"),
    ("java/io/File", "getCanonicalFile", "()Ljava/io/File;"),
    ("java/io/File", "getCanonicalPath", "()Ljava/lang/String;"),
    ("java/io/File", "getFreeSpace", "()J"),
    ("java/io/File", "getName", "()Ljava/lang/String;"),
    ("java/io/File", "getParent", "()Ljava/lang/String;"),
    ("java/io/File", "getParentFile", "()Ljava/io/File;"),
    ("java/io/File", "getPath", "()Ljava/lang/String;"),
    ("java/io/File", "getTotalSpace", "()J"),
    ("java/io/File", "getUsableSpace", "()J"),
    ("java/io/File", "hashCode", "()I"),
    ("java/io/File", "isAbsolute", "()Z"),
    ("java/io/File", "isDirectory", "()Z"),
    ("java/io/File", "isFile", "()Z"),
    ("java/io/File", "isHidden", "()Z"),
    ("java/io/File", "lastModified", "()J"),
    ("java/io/File", "length", "()J"),
    ("java/io/File", "list", "()[Ljava/lang/String;"),
    (
        "java/io/File",
        "list",
        "(Ljava/io/FilenameFilter;)[Ljava/lang/String;",
    ),
    ("java/io/File", "listFiles", "()[Ljava/io/File;"),
    (
        "java/io/File",
        "listFiles",
        "(Ljava/io/FileFilter;)[Ljava/io/File;",
    ),
    (
        "java/io/File",
        "listFiles",
        "(Ljava/io/FilenameFilter;)[Ljava/io/File;",
    ),
    ("java/io/File", "listRoots", "()[Ljava/io/File;"),
    ("java/io/File", "mkdir", "()Z"),
    ("java/io/File", "mkdirs", "()Z"),
    ("java/io/File", "renameTo", "(Ljava/io/File;)Z"),
    ("java/io/File", "setExecutable", "(Z)Z"),
    ("java/io/File", "setExecutable", "(ZZ)Z"),
    ("java/io/File", "setLastModified", "(J)Z"),
    ("java/io/File", "setReadOnly", "()Z"),
    ("java/io/File", "setReadable", "(Z)Z"),
    ("java/io/File", "setReadable", "(ZZ)Z"),
    ("java/io/File", "setWritable", "(Z)Z"),
    ("java/io/File", "setWritable", "(ZZ)Z"),
    ("java/io/File", "toPath", "()Ljava/nio/file/Path;"),
    ("java/io/File", "toString", "()Ljava/lang/String;"),
    ("java/io/File", "toURI", "()Ljava/net/URI;"),
    ("java/io/FilterOutputStream", "close", "()V"),
    ("java/io/FilterOutputStream", "flush", "()V"),
    ("java/io/FilterOutputStream", "write", "(I)V"),
    ("java/io/FilterOutputStream", "write", "([B)V"),
    ("java/io/FilterOutputStream", "write", "([BII)V"),
    (
        "java/nio/ByteBuffer",
        "allocate",
        "(I)Ljava/nio/ByteBuffer;",
    ),
    (
        "java/nio/ByteBuffer",
        "allocateDirect",
        "(I)Ljava/nio/ByteBuffer;",
    ),
    ("java/nio/ByteBuffer", "array", "()[B"),
    ("java/nio/ByteBuffer", "arrayOffset", "()I"),
    ("java/nio/ByteBuffer", "capacity", "()I"),
    ("java/nio/ByteBuffer", "clear", "()Ljava/nio/Buffer;"),
    ("java/nio/ByteBuffer", "clear", "()Ljava/nio/ByteBuffer;"),
    (
        "java/nio/ByteBuffer",
        "compareTo",
        "(Ljava/nio/ByteBuffer;)I",
    ),
    ("java/nio/ByteBuffer", "equals", "(Ljava/lang/Object;)Z"),
    ("java/nio/ByteBuffer", "flip", "()Ljava/nio/Buffer;"),
    ("java/nio/ByteBuffer", "flip", "()Ljava/nio/ByteBuffer;"),
    ("java/nio/ByteBuffer", "get", "([B)Ljava/nio/ByteBuffer;"),
    ("java/nio/ByteBuffer", "hasArray", "()Z"),
    ("java/nio/ByteBuffer", "hasRemaining", "()Z"),
    ("java/nio/ByteBuffer", "hashCode", "()I"),
    ("java/nio/ByteBuffer", "limit", "()I"),
    ("java/nio/ByteBuffer", "limit", "(I)Ljava/nio/Buffer;"),
    ("java/nio/ByteBuffer", "limit", "(I)Ljava/nio/ByteBuffer;"),
    ("java/nio/ByteBuffer", "mark", "()Ljava/nio/Buffer;"),
    ("java/nio/ByteBuffer", "mark", "()Ljava/nio/ByteBuffer;"),
    ("java/nio/ByteBuffer", "order", "()Ljava/nio/ByteOrder;"),
    (
        "java/nio/ByteBuffer",
        "order",
        "(Ljava/nio/ByteOrder;)Ljava/nio/ByteBuffer;",
    ),
    ("java/nio/ByteBuffer", "position", "()I"),
    ("java/nio/ByteBuffer", "position", "(I)Ljava/nio/Buffer;"),
    (
        "java/nio/ByteBuffer",
        "position",
        "(I)Ljava/nio/ByteBuffer;",
    ),
    (
        "java/nio/ByteBuffer",
        "put",
        "(Ljava/nio/ByteBuffer;)Ljava/nio/ByteBuffer;",
    ),
    ("java/nio/ByteBuffer", "put", "([B)Ljava/nio/ByteBuffer;"),
    ("java/nio/ByteBuffer", "put", "([BII)Ljava/nio/ByteBuffer;"),
    ("java/nio/ByteBuffer", "remaining", "()I"),
    ("java/nio/ByteBuffer", "reset", "()Ljava/nio/Buffer;"),
    ("java/nio/ByteBuffer", "rewind", "()Ljava/nio/Buffer;"),
    ("java/nio/ByteBuffer", "rewind", "()Ljava/nio/ByteBuffer;"),
    ("java/nio/ByteBuffer", "wrap", "([B)Ljava/nio/ByteBuffer;"),
    ("java/nio/ByteBuffer", "wrap", "([BII)Ljava/nio/ByteBuffer;"),
    (
        "java/nio/ByteBufferAsCharBufferB",
        "order",
        "()Ljava/nio/ByteOrder;",
    ),
    (
        "java/nio/ByteBufferAsCharBufferL",
        "order",
        "()Ljava/nio/ByteOrder;",
    ),
    (
        "java/nio/ByteBufferAsCharBufferRB",
        "order",
        "()Ljava/nio/ByteOrder;",
    ),
    (
        "java/nio/ByteBufferAsCharBufferRL",
        "order",
        "()Ljava/nio/ByteOrder;",
    ),
];

/// Lane 4 wave 2, 2026-09-11: the nine `ValueLayouts$Of*Impl` carriers.
///
/// **An EIGHTH table**, and the first under `jdk/internal/foreign/`. Wave 1's
/// note said "nothing under it reached the candidate set, so its prefix is not
/// admitted either" — that was a statement about the instrument, not about the
/// package. The lane's ledger put 479 rows in "no probe in this tree invokes
/// it" and named this shape as over half of them; one probe,
/// `L4FfmLayoutSweep.java`, moved every one of these into the
/// funnel in a single pass.
///
/// # The funnel
///
/// 146 registrations over nine classes, and the census taken from that probe's
/// own run reports `invocations > 0` on **all 146**, with the schema-5
/// `invocations_complete` bit set on every row — so this is not the lower bound
/// that bucket usually is. 83 are bucket A and 63 bucket B (`byteSize`,
/// `byteAlignment`, `name`, `order`, `carrier`, `byteOffset`, inherited from
/// `AbstractLayout` and `ValueLayouts$AbstractValueLayout`, which is where the
/// real bodies live).
///
/// # What the retirement does, row by row
///
/// Measured on `vm-l4ffm-fix`, one binary against itself, with the dial scoped
/// to exactly this wave (`CRATONVM_ENFORCE_NATIVE_SHADOW=jdk/internal/foreign/
/// layout/ValueLayouts$`), against HotSpot 25 over the probe's 359 rows:
///
/// ```text
///   unarmed --real-jdk          25 rows differ
///   armed on this wave          25 rows differ    <- the SAME number
/// ```
///
/// The same number and **not the same rows**, which is the whole finding. 19
/// rows the native answered wrongly become right, because the real bodies throw
/// the JDK's own text and render the JDK's own string:
///
/// ```text
///   withByteAlignment(3)   "Invalid alignment constraint: 3" -> "Invalid alignment: 3"     x9
///   byteOffset(groupElement)  a home-grown message -> "Bad layout path: ..."               x9
///   ADDRESS.withTargetLayout(JAVA_INT).toString()   "a8" -> "a8:i4"                        x1
/// ```
///
/// and 19 become wrong, every one of them `varHandle`. **So `varHandle` is
/// carved out and this table is 137 rows, not 146.** The real
/// `AbstractValueLayout.varHandle()` runs `Utils.makeSegmentViewVarHandle`,
/// which on this VM ends in `NoClassDefFoundError: java/lang/invoke/
/// BoundMethodHandle` or hands back a CratonVM VarHandle with no variable-type
/// metadata — `varType()` and `coordinateTypes()` then refuse, and a
/// `vh.set`/`vh.get` round trip through a heap segment reads `size=0`. That is
/// a MethodHandle-infrastructure gap, not a layout one, and it is the only
/// method of the thirteen that the real body cannot service.
///
/// With `varHandle` out, the arm is the floor minus those 19: **six rows**, and
/// none of them is on a class in this table.
///
/// # The precondition, which is the commit before this one
///
/// This wave does not stand on its own. The carrier fix in
/// `native-builtins/src/phases_late/foreign_ffm.rs` had to land first, for the
/// same reason wave 1's `java.io.File` fix had to: **a layout minted by this VM
/// did not hold what the real bodies read.** `AbstractLayout.name` is an
/// `Optional<String>` and the mint wrote a bare reference; `carrier` was never
/// written at all, and the native `carrier()` derived its answer from the class
/// NAME instead — so the method answered correctly while the field behind it
/// was null. Armed before that fix, this same scope took the probe from 100
/// differing lines to **358**; after it, to the floor. The dial did not find
/// the defect and could not have: it names a family, and the failure was
/// underneath all nine of them.
///
/// # What is held back, and why
///
/// The prefix admits `jdk/internal/foreign/layout/`; the table decides. Of the
/// 251 bucket-A/B rows under `jdk/internal/foreign` in this census:
///
///  * **`varHandle` (9)** — above.
///  * **The group layouts (35): `StructLayoutImpl`, `UnionLayoutImpl`,
///    `SequenceLayoutImpl`, `PaddingLayoutImpl`.** One carrier defect, not
///    thirteen: `AbstractGroupLayout.elements` is declared
///    `java.util.List<MemoryLayout>` and this VM stores a java ARRAY in that
///    field. Armed, `memberLayouts()` answers 0 where the oracle answers 2,
///    `byteOffset(groupElement("c"))` cannot resolve a member that is plainly
///    there, and every group `toString` dies in
///    `NoSuchMethodError: 'int java.lang.foreign.MemoryLayout.size()'`. It is
///    the same SHAPE as the defect this wave fixed and it wants the same
///    treatment — a real `List`, written at the mint — which is a change to the
///    group factories and belongs in its own commit with its own measurement.
///  * **The segment, arena and session carriers (70).** Blocked by a decision
///    on record, not by a missing measurement:
///    `docs/feature-designs/native-dispatch-architecture.md`
///    settles that `cratonvm/internal/foreign/MemorySegmentImpl` is the VM's own
///    allocation shape and is laid out DELIBERATELY unlike
///    `AbstractMemorySegmentImpl`, whose `length`/`readOnly`/`scope` would alias
///    the carrier's `ptr`/`size`/`arena`. Retiring one of these would run a real
///    body over those three slots. That is not a wave that needs screening; it
///    is a wave that must not be run while that decision stands.
///  * **`java/lang/foreign/*`** — the interfaces. Bucket C: abstract in the
///    image, dispatched through no door, and still named by the FFM arm of
///    `force_native_over_real_jdk_bytecode`, which this commit narrows to
///    exactly them.
///
/// # The residual this wave found and did not fix
///
/// COMPATIBLE mode keeps a second, independent copy of the mint:
/// `make_prepared_value_layout` in `vm/src/vm/vm_util.rs`, the preseed that
/// gives `ValueLayout.JAVA_INT` and its fifteen siblings their statics before
/// `<clinit>`. It resolves `byteSize`, `byteAlignment` and `name` by name and
/// writes **three of the five** real fields; `carrier` and `order` stay null,
/// and `ValueLayout.JAVA_INT.withName("k").equals(...)` still throws
/// `NullPointerException` there. `--real-jdk` drops that preseed entirely and
/// runs the real `<clinit>`, which is why the strict arm is clean and the
/// compatible one is not (50 differing lines against 68). **It does not affect
/// this table**: `NativeKind::allowed_in(Compatible)` is `true` for every kind,
/// so a retired triple still dispatches its native in compatible mode and the
/// re-tag is a `--real-jdk` change only.
///
/// Measured on **linux/x86_64 against JDK 25**, the platform both shell gates
/// in this campaign are keyed to.
pub(in crate::retired_shadow) static RETIRED_SHADOW_L4_FFM_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
        "byteAlignment",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
        "byteOffset",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
        "byteSize",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
        "carrier",
        "()Ljava/lang/Class;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
        "name",
        "()Ljava/util/Optional;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
        "order",
        "()Ljava/nio/ByteOrder;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
        "targetLayout",
        "()Ljava/util/Optional;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/AddressLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/AddressLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
        "withOrder",
        "(Ljava/nio/ByteOrder;)Ljava/lang/foreign/AddressLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
        "withOrder",
        "(Ljava/nio/ByteOrder;)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
        "withTargetLayout",
        "(Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/AddressLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfBooleanImpl",
        "byteAlignment",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfBooleanImpl",
        "byteOffset",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfBooleanImpl",
        "byteSize",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfBooleanImpl",
        "carrier",
        "()Ljava/lang/Class;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfBooleanImpl",
        "name",
        "()Ljava/util/Optional;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfBooleanImpl",
        "order",
        "()Ljava/nio/ByteOrder;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfBooleanImpl",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfBooleanImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfBooleanImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/ValueLayout$OfBoolean;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfBooleanImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfBooleanImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfBooleanImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/ValueLayout$OfBoolean;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfBooleanImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfBooleanImpl",
        "withOrder",
        "(Ljava/nio/ByteOrder;)Ljava/lang/foreign/ValueLayout$OfBoolean;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfBooleanImpl",
        "withOrder",
        "(Ljava/nio/ByteOrder;)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfByteImpl",
        "byteAlignment",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfByteImpl",
        "byteOffset",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfByteImpl",
        "byteSize",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfByteImpl",
        "carrier",
        "()Ljava/lang/Class;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfByteImpl",
        "name",
        "()Ljava/util/Optional;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfByteImpl",
        "order",
        "()Ljava/nio/ByteOrder;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfByteImpl",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfByteImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfByteImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/ValueLayout$OfByte;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfByteImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfByteImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfByteImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/ValueLayout$OfByte;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfByteImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfByteImpl",
        "withOrder",
        "(Ljava/nio/ByteOrder;)Ljava/lang/foreign/ValueLayout$OfByte;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfByteImpl",
        "withOrder",
        "(Ljava/nio/ByteOrder;)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfCharImpl",
        "byteAlignment",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfCharImpl",
        "byteOffset",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfCharImpl",
        "byteSize",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfCharImpl",
        "carrier",
        "()Ljava/lang/Class;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfCharImpl",
        "name",
        "()Ljava/util/Optional;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfCharImpl",
        "order",
        "()Ljava/nio/ByteOrder;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfCharImpl",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfCharImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfCharImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/ValueLayout$OfChar;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfCharImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfCharImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfCharImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/ValueLayout$OfChar;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfCharImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfCharImpl",
        "withOrder",
        "(Ljava/nio/ByteOrder;)Ljava/lang/foreign/ValueLayout$OfChar;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfCharImpl",
        "withOrder",
        "(Ljava/nio/ByteOrder;)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfDoubleImpl",
        "byteAlignment",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfDoubleImpl",
        "byteOffset",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfDoubleImpl",
        "byteSize",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfDoubleImpl",
        "carrier",
        "()Ljava/lang/Class;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfDoubleImpl",
        "name",
        "()Ljava/util/Optional;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfDoubleImpl",
        "order",
        "()Ljava/nio/ByteOrder;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfDoubleImpl",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfDoubleImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfDoubleImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/ValueLayout$OfDouble;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfDoubleImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfDoubleImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfDoubleImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/ValueLayout$OfDouble;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfDoubleImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfDoubleImpl",
        "withOrder",
        "(Ljava/nio/ByteOrder;)Ljava/lang/foreign/ValueLayout$OfDouble;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfDoubleImpl",
        "withOrder",
        "(Ljava/nio/ByteOrder;)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfFloatImpl",
        "byteAlignment",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfFloatImpl",
        "byteOffset",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfFloatImpl",
        "byteSize",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfFloatImpl",
        "carrier",
        "()Ljava/lang/Class;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfFloatImpl",
        "name",
        "()Ljava/util/Optional;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfFloatImpl",
        "order",
        "()Ljava/nio/ByteOrder;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfFloatImpl",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfFloatImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfFloatImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/ValueLayout$OfFloat;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfFloatImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfFloatImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfFloatImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/ValueLayout$OfFloat;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfFloatImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfFloatImpl",
        "withOrder",
        "(Ljava/nio/ByteOrder;)Ljava/lang/foreign/ValueLayout$OfFloat;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfFloatImpl",
        "withOrder",
        "(Ljava/nio/ByteOrder;)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
        "byteAlignment",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
        "byteOffset",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
        "byteSize",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
        "carrier",
        "()Ljava/lang/Class;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
        "name",
        "()Ljava/util/Optional;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
        "order",
        "()Ljava/nio/ByteOrder;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/ValueLayout$OfInt;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/ValueLayout$OfInt;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
        "withOrder",
        "(Ljava/nio/ByteOrder;)Ljava/lang/foreign/ValueLayout$OfInt;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
        "withOrder",
        "(Ljava/nio/ByteOrder;)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
        "byteAlignment",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
        "byteOffset",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
        "byteSize",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
        "carrier",
        "()Ljava/lang/Class;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
        "name",
        "()Ljava/util/Optional;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
        "order",
        "()Ljava/nio/ByteOrder;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/ValueLayout$OfLong;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/ValueLayout$OfLong;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
        "withOrder",
        "(Ljava/nio/ByteOrder;)Ljava/lang/foreign/ValueLayout$OfLong;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
        "withOrder",
        "(Ljava/nio/ByteOrder;)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfShortImpl",
        "byteAlignment",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfShortImpl",
        "byteOffset",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfShortImpl",
        "byteSize",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfShortImpl",
        "carrier",
        "()Ljava/lang/Class;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfShortImpl",
        "name",
        "()Ljava/util/Optional;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfShortImpl",
        "order",
        "()Ljava/nio/ByteOrder;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfShortImpl",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfShortImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfShortImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/ValueLayout$OfShort;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfShortImpl",
        "withByteAlignment",
        "(J)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfShortImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfShortImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/ValueLayout$OfShort;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfShortImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/ValueLayout;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfShortImpl",
        "withOrder",
        "(Ljava/nio/ByteOrder;)Ljava/lang/foreign/ValueLayout$OfShort;",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfShortImpl",
        "withOrder",
        "(Ljava/nio/ByteOrder;)Ljava/lang/foreign/ValueLayout;",
    ),
];

/// Lane 4 wave 3, 2026-09-12: the four GROUP carriers.
///
/// `StructLayoutImpl`, `UnionLayoutImpl`, `SequenceLayoutImpl` and
/// `PaddingLayoutImpl` — the half of `jdk/internal/foreign/layout/` wave 2 held
/// back, under the prefix wave 2 admitted. 28 rows of 31: `varHandle` is carved
/// out on all three classes that register it, for the reason wave 2 carved it
/// out on the nine value layouts.
///
/// # The carrier was the blocker, and it was ONE defect for all four classes
///
/// `jdk.internal.foreign.layout.AbstractGroupLayout` declares
/// `List<MemoryLayout> elements` and this VM stored a bare ARRAY in it; `kind`
/// and `minByteAlignment` were never written at all. `memberLayouts()` is
/// `return elements;` — one `getfield` — so the first real body to touch a
/// group got an array where the JDK's own code calls `List` methods:
///
/// ```text
///   struct.memberLayouts().size()      0          (oracle 2)
///   struct.byteOffset(groupElement)    cannot resolve a member plainly there
///   struct.toString()                  NoSuchMethodError: MemoryLayout.size()
/// ```
///
/// Fixed in the commit before this table, through one reader and one writer:
/// `p67_group_members` decodes whichever shape the field holds and returns the
/// COUNT beside the array (an `ArrayList` has capacity past its size, and
/// reading `array_length` would invent trailing members); `p67_group_set_members`
/// writes the list; `p67_group_set_kind` writes `kind` from the enum's own
/// statics, so there is one `STRUCT` object in the VM and `==` on it answers
/// what the JDK expects.
///
/// **The list is UNMODIFIABLE and that is measured, not tidy.** HotSpot answers
/// `UnsupportedOperationException` to `memberLayouts().add(...)`; since the
/// retired accessor returns the field itself, an `ArrayList` here would hand
/// out a mutable view of a layout's members.
///
/// # The funnel
///
/// 31 distinct triples over the four classes, every one of them reached by
/// `L4FfmLayoutSweep.java` — six were cold on the first pass
/// (`PaddingLayoutImpl.byteAlignment`, the three `withName` covariant bridges
/// at the `MemoryLayout` static type, and `UnionLayoutImpl`'s `byteOffset` and
/// `varHandle`) and the probe was extended until none was. A row no probe
/// invokes is a row the funnel must not take.
///
/// # What the retirement does
///
/// One binary against itself, the group table un-retired with
/// `CRATONVM_UNRETIRE_NATIVE_SHADOW` naming the four classes -- which prints
/// `7 + 7 + 8 + 6 = 28 table row(s)` and is the arm's own receipt that it armed
/// this table and not its neighbour:
///
/// ```text
///   control (origin/dev)                    13 rows differ
///   carrier fix, group table un-retired      3
///   carrier fix + this table                 2
/// ```
///
/// **The retirement fixes one row and breaks none.** `byteOffset` on a padding
/// layout throws the JDK's own `Bad layout path: attempting to select a group
/// element from a non-group layout: x4` where the native answered its own
/// wording. Eleven of the control's thirteen were the carrier, which is the
/// commit before this one; the two left over are neither this wave's nor a
/// group's, and both are in `varHandle`:
///
///  * `ADDRESS.varHandle().varType()` answers `long` where the oracle says
///    `MemorySegment`;
///  * a union's `varHandle` ACCEPTS a misaligned access HotSpot refuses -- a
///    missing refusal, which is the silent-wrong-answer shape this lane owns,
///    in the one method carved out of both FFM waves.
///
/// **28 refusals, 0 survivors**, and on the control all 28 were `native-won`:
/// a native winning over real bytecode that was there the whole time.
///
/// Measured on **linux/x86_64 against JDK 25**.
pub(in crate::retired_shadow) static RETIRED_SHADOW_L4_FFM_GROUP_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "jdk/internal/foreign/layout/PaddingLayoutImpl",
        "byteAlignment",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/PaddingLayoutImpl",
        "byteOffset",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)J",
    ),
    (
        "jdk/internal/foreign/layout/PaddingLayoutImpl",
        "byteSize",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/PaddingLayoutImpl",
        "name",
        "()Ljava/util/Optional;",
    ),
    (
        "jdk/internal/foreign/layout/PaddingLayoutImpl",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "jdk/internal/foreign/layout/PaddingLayoutImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/SequenceLayoutImpl",
        "byteAlignment",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/SequenceLayoutImpl",
        "byteOffset",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)J",
    ),
    (
        "jdk/internal/foreign/layout/SequenceLayoutImpl",
        "byteSize",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/SequenceLayoutImpl",
        "elementCount",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/SequenceLayoutImpl",
        "elementLayout",
        "()Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/SequenceLayoutImpl",
        "name",
        "()Ljava/util/Optional;",
    ),
    (
        "jdk/internal/foreign/layout/SequenceLayoutImpl",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "jdk/internal/foreign/layout/SequenceLayoutImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/StructLayoutImpl",
        "byteAlignment",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/StructLayoutImpl",
        "byteOffset",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)J",
    ),
    (
        "jdk/internal/foreign/layout/StructLayoutImpl",
        "byteSize",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/StructLayoutImpl",
        "memberLayouts",
        "()Ljava/util/List;",
    ),
    (
        "jdk/internal/foreign/layout/StructLayoutImpl",
        "name",
        "()Ljava/util/Optional;",
    ),
    (
        "jdk/internal/foreign/layout/StructLayoutImpl",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "jdk/internal/foreign/layout/StructLayoutImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;",
    ),
    (
        "jdk/internal/foreign/layout/UnionLayoutImpl",
        "byteAlignment",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/UnionLayoutImpl",
        "byteOffset",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)J",
    ),
    (
        "jdk/internal/foreign/layout/UnionLayoutImpl",
        "byteSize",
        "()J",
    ),
    (
        "jdk/internal/foreign/layout/UnionLayoutImpl",
        "memberLayouts",
        "()Ljava/util/List;",
    ),
    (
        "jdk/internal/foreign/layout/UnionLayoutImpl",
        "name",
        "()Ljava/util/Optional;",
    ),
    (
        "jdk/internal/foreign/layout/UnionLayoutImpl",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "jdk/internal/foreign/layout/UnionLayoutImpl",
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;",
    ),
];

