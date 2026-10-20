import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.Linker;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.SymbolLookup;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.VarHandle;
import java.util.ArrayList;
import java.util.List;
import java.util.Optional;

/**
 * JDK-only corpus: the Panama / FFM surface -- {@code Linker}, downcalls,
 * {@code Arena}, {@code MemorySegment}, {@code ValueLayout} var handles.
 *
 * <h2>Why this class exists</h2>
 *
 * A 33-probe reachability screen of ordinary Java under {@code --jdk-only}
 * (2026-08-12, HotSpot 25 green on all 33) found FFM entirely dead here:
 *
 * <pre>
 * FAIL FFM downcall (DowncallHandle)
 *   -&gt; java.lang.NoClassDefFoundError: java/lang/foreign/DowncallHandle
 * </pre>
 *
 * {@code java.lang.foreign.DowncallHandle} is a class no real JDK declares --
 * it was CratonVM's own invention, minted as the carrier that
 * {@code Linker.downcallHandle} handed back in place of the
 * {@code java.lang.invoke.MethodHandle} the method is declared to return.
 * Strict mode refuses to fabricate a class no image declares, correctly, so
 * every downcall died at the application's call site. The refusal was right;
 * the survival of its caller was the defect. The carrier is now a real
 * {@code MethodHandle} (see {@code native-builtins/src/panama.rs},
 * {@code alloc_downcall_handle}).
 *
 * <h2>What that history dictates about the checks below</h2>
 *
 * Nothing here is asserted with {@code != null}. Two defects survived this
 * year behind {@code != null} and length checks, and this surface is
 * especially prone to it: a fabricated carrier answers every metadata query
 * plausibly and computes nothing. So:
 *
 * <ul>
 *   <li>Every downcall asserts a COMPUTED value that only the real libc body
 *       can produce, and asserts it for more than one input -- {@code strlen}
 *       is checked at three lengths and {@code abs} on both signs, so a stub
 *       returning a constant (or the identity) fails.</li>
 *   <li>{@link #handleIsARealMethodHandle} asserts the carrier's TYPE, not its
 *       existence. {@code type()} on the old carrier was served by a native on
 *       the invented class; on a real {@code MethodHandle} it is the JDK's own
 *       {@code type} field, so a carrier that carried the function address
 *       where the {@code MethodType} belongs reports {@code ()void} here.</li>
 *   <li>The same step erases the handle to {@code Object} and casts it back.
 *       That is a {@code checkcast} to {@code java.lang.invoke.MethodHandle},
 *       which the VM used to satisfy only via a hard-coded special case for
 *       the invented class name.</li>
 *   <li>Every refusal asserts the JDK's exception TYPE
 *       ({@code IllegalStateException} for a closed arena,
 *       {@code IndexOutOfBoundsException} for an out-of-bounds access), and
 *       each is paired with a positive control so a VM that threw from every
 *       access could not pass.</li>
 * </ul>
 *
 * <h2>Host portability</h2>
 *
 * The symbols used ({@code strlen}, {@code abs}) are C runtime entry points
 * present on Linux and Windows alike, but what {@code defaultLookup()} covers
 * differs. {@link #lookup} therefore tries the default lookup first and falls
 * back to an explicit {@code SymbolLookup.libraryLookup}; the arm that ran is
 * printed on the {@code CK RJdkForeign lookup} line, so a cross-VM diff that
 * disagrees says which arm each side took.
 *
 * <p>Determinism: no addresses are printed, no identity hashes, no timing.
 * {@code MemorySegment.address()} is only ever compared against 0.
 *
 * <h2>Steps, not a straight line</h2>
 *
 * Same reason as {@code RJdkHandles}: while the carrier was refused, the FIRST
 * downcall threw and every later claim -- arena scoping, segment round trips,
 * var handles, none of which need a downcall -- went unreported. Each
 * independent claim runs inside {@link #step}, so one run names every broken
 * area instead of the first.
 */
public class RJdkForeign {
    static int checks;
    static int steps;
    static final List<String> failures = new ArrayList<>();

    /** Which lookup arm resolved the C runtime; reported on a CK line. */
    static String lookupArm = "unresolved";

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    interface Body {
        void run() throws Throwable;
    }

    /**
     * Run one INDEPENDENT claim. A failure is named, recorded and survived; the
     * next claim still runs.
     *
     * <p>The printed line is deliberately not a {@code CK } line: {@code run.sh}
     * keeps only {@code PASS}/{@code CK} lines for its cross-VM diff, and a
     * failing run is already red on {@code rc} before that diff is reached.
     */
    static void step(String name, Body body) {
        steps++;
        try {
            body.run();
        } catch (Throwable t) {
            failures.add(name);
            System.out.println("FAIL RJdkForeign step " + name + ": "
                    + t.getClass().getName() + ": " + t.getMessage());
        }
    }

    static boolean isWindows() {
        return System.getProperty("os.name", "").toLowerCase().contains("win");
    }

    /**
     * A {@code SymbolLookup} that resolves the C runtime on this host.
     *
     * <p>{@code defaultLookup()} is the contract-bearing path and is tried
     * first on both platforms. The fallback exists because the set of
     * libraries the default lookup covers is implementation-defined: the JDK
     * documents it as "a set of commonly used libraries", not a fixed list.
     * Falling back rather than failing keeps a difference in that set from
     * being reported as a downcall defect -- and {@link #lookupArm} records
     * which happened, so the two are still distinguishable in the output.
     */
    static SymbolLookup lookup() {
        SymbolLookup dflt = Linker.nativeLinker().defaultLookup();
        if (dflt.find("strlen").isPresent() && dflt.find("abs").isPresent()) {
            lookupArm = "defaultLookup";
            return dflt;
        }
        String lib = isWindows() ? "msvcrt" : "c";
        lookupArm = "libraryLookup:" + lib;
        // Arena.global() is never closed, so the returned lookup's symbols stay
        // valid for the rest of the run -- a confined arena here would hand back
        // segments that are already dead by the time a downcall used one.
        return SymbolLookup.libraryLookup(lib, Arena.global());
    }

    static MemorySegment symbol(SymbolLookup lk, String name) {
        Optional<MemorySegment> found = lk.find(name);
        check(found.isPresent(), "SymbolLookup must resolve " + name
                + " (arm=" + lookupArm + ")");
        MemorySegment sym = found.get();
        // A symbol that resolved to the null pointer is a lookup that answered
        // "yes" without finding anything -- the fabricated-success shape.
        check(sym.address() != 0L, name + " must resolve to a non-null address");
        return sym;
    }

    static MethodHandle strlenHandle(SymbolLookup lk) {
        return Linker.nativeLinker().downcallHandle(symbol(lk, "strlen"),
                FunctionDescriptor.of(ValueLayout.JAVA_LONG, ValueLayout.ADDRESS));
    }

    /**
     * The downcall itself, at three lengths.
     *
     * <p>Three inputs rather than one: {@code strlen("abcd") == 4} alone is
     * satisfied by a VM that returns the argument's segment size, by one that
     * returns a cached constant, and by one that returns the descriptor's
     * parameter count. The empty string and a 26-byte string separate all
     * three.
     */
    static void downcall() throws Throwable {
        SymbolLookup lk = lookup();
        MethodHandle strlen = strlenHandle(lk);
        try (Arena arena = Arena.ofConfined()) {
            check((long) strlen.invokeExact(arena.allocateFrom("abcd")) == 4L,
                    "strlen(\"abcd\") must be 4");
            check((long) strlen.invokeExact(arena.allocateFrom("")) == 0L,
                    "strlen(\"\") must be 0");
            check((long) strlen.invokeExact(
                    arena.allocateFrom("abcdefghijklmnopqrstuvwxyz")) == 26L,
                    "strlen of a 26-character string must be 26");
            // NUL terminates: strlen must stop at the embedded zero, not run to
            // the segment's end. A VM that answered from byteSize() passes every
            // check above and fails this one.
            check((long) strlen.invokeExact(arena.allocateFrom("ab\0cd")) == 2L,
                    "strlen must stop at an embedded NUL");
        }
        System.out.println("CK RJdkForeign lookup arm=" + lookupArm
                + " os=" + (isWindows() ? "windows" : "unix"));
    }

    /**
     * A second symbol with a different signature shape.
     *
     * <p>{@code strlen} is {@code (ADDRESS)JAVA_LONG}; {@code abs} is
     * {@code (JAVA_INT)JAVA_INT}. A VM whose marshalling only ever handled one
     * carrier width passes one of these steps and not the other, which is the
     * whole reason there are two.
     */
    static void downcallInt() throws Throwable {
        SymbolLookup lk = lookup();
        MethodHandle abs = Linker.nativeLinker().downcallHandle(symbol(lk, "abs"),
                FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_INT));
        check((int) abs.invokeExact(-42) == 42, "abs(-42) must be 42");
        // The positive control. Without it, a VM that negated unconditionally --
        // or one that returned a constant 42 -- passes the line above.
        check((int) abs.invokeExact(7) == 7, "abs(7) must be 7");
        check((int) abs.invokeExact(0) == 0, "abs(0) must be 0");
    }

    /**
     * A heap-array {@code MemorySegment} handed straight to a downcall must be
     * REFUSED, not silently marshalled as a null pointer.
     *
     * <p>MEASURED on HotSpot 25.0.3+9-LTS ({@code FfmRepro2.java}):
     * {@code strlen.invoke(MemorySegment.ofArray(new byte[]{'h','i',0}))}
     * throws {@code IllegalArgumentException: Heap segment not allowed: ...}.
     * A heap array's bytes live on the managed heap and have no machine
     * address for native code to dereference; CratonVM used to answer that
     * with address 0 (a legitimate C null), which made the call silently
     * receive {@code NULL} instead of saying why. The positive control right
     * after (an Arena-backed segment) is what stops a VM that refuses every
     * downcall from passing this step.
     */
    static void heapSegmentDowncallRefusal() throws Throwable {
        SymbolLookup lk = lookup();
        MethodHandle strlen = strlenHandle(lk);

        boolean threw = false;
        try {
            long unreachable = (long) strlen.invokeExact(
                    (MemorySegment) MemorySegment.ofArray(new byte[] { 'h', 'i', 0 }));
            check(false, "unreachable: strlen of a heap segment returned "
                    + unreachable + " instead of throwing");
        } catch (IllegalArgumentException expected) {
            threw = true;
        }
        check(threw, "a heap-array MemorySegment passed to a downcall must raise "
                + "IllegalArgumentException");

        // POSITIVE CONTROL: an Arena-backed segment with the identical bytes
        // must still succeed, immediately after the refusal above.
        try (Arena arena = Arena.ofConfined()) {
            check((long) strlen.invokeExact(arena.allocateFrom("hi")) == 2L,
                    "an Arena-backed segment must still downcall after the "
                            + "heap-segment refusal");
        }
    }

    /**
     * The carrier {@code Linker.downcallHandle} hands back is a real
     * {@code java.lang.invoke.MethodHandle}.
     *
     * <p>This is the step that gates the P1-E fix directly. Every claim here
     * was answered by a VM special case keyed on the invented class name
     * {@code java/lang/foreign/DowncallHandle} before the carrier changed.
     */
    static void handleIsARealMethodHandle() throws Throwable {
        SymbolLookup lk = lookup();
        MethodHandle strlen = strlenHandle(lk);

        // type() must describe the FunctionDescriptor, not the Object[] invoker
        // the carrier is dispatched through and not the ()void a handle with an
        // unpopulated `type` field reports.
        check(strlen.type().returnType() == long.class,
                "a (ADDRESS)JAVA_LONG downcall must report a long return type, got "
                        + strlen.type());
        check(strlen.type().parameterCount() == 1,
                "a one-argument downcall must report parameterCount 1, got "
                        + strlen.type());
        check(strlen.type().parameterType(0) == MemorySegment.class,
                "an ADDRESS parameter must carry as MemorySegment, got "
                        + strlen.type());

        // Erase and cast back: a checkcast to java.lang.invoke.MethodHandle.
        Object erased = strlen;
        check(erased instanceof MethodHandle,
                "downcallHandle's result must BE a java.lang.invoke.MethodHandle");
        MethodHandle back = (MethodHandle) erased;
        try (Arena arena = Arena.ofConfined()) {
            check((long) back.invokeExact(arena.allocateFrom("xyz")) == 3L,
                    "a downcall handle must still invoke after an Object round trip");
        }

        // Two handles for the same symbol are distinct objects with equal types.
        // Equal `type()` is the claim; a VM that returned one shared carrier for
        // every downcall would satisfy it and fail the identity half.
        MethodHandle other = strlenHandle(lk);
        check(other != strlen, "each downcallHandle call must mint its own handle");
        check(other.type().equals(strlen.type()),
                "two handles for the same descriptor must have equal types");
    }

    /**
     * {@code Arena.ofConfined()} scoping, both polarities.
     *
     * <p>The refusal is asserted by TYPE. The positive control immediately
     * before it is what stops a VM that throws from every segment access from
     * passing.
     */
    static void arenaScoping() throws Throwable {
        MemorySegment escaped;
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment seg = arena.allocate(ValueLayout.JAVA_INT);
            seg.set(ValueLayout.JAVA_INT, 0, 0x5EED);
            // POSITIVE CONTROL: inside the scope the access must work.
            check(seg.get(ValueLayout.JAVA_INT, 0) == 0x5EED,
                    "a live confined segment must read back what was written");
            check(seg.byteSize() == 4L, "allocate(JAVA_INT) must have byteSize 4");
            check(seg.scope().isAlive(), "an open arena's scope must be alive");
            escaped = seg;
        }

        check(!escaped.scope().isAlive(), "a closed arena's scope must not be alive");
        boolean threw = false;
        try {
            int leaked = escaped.get(ValueLayout.JAVA_INT, 0);
            check(leaked == -1, "unreachable: read " + leaked + " from a closed arena");
        } catch (IllegalStateException expected) {
            threw = true;
        }
        check(threw, "a use-after-close read must raise IllegalStateException");

        threw = false;
        try {
            escaped.set(ValueLayout.JAVA_INT, 0, 1);
        } catch (IllegalStateException expected) {
            threw = true;
        }
        check(threw, "a use-after-close write must raise IllegalStateException");

        // Closing twice is also an IllegalStateException, and it is a distinct
        // path from an access: the arena's own state, not a segment's.
        Arena twice = Arena.ofConfined();
        twice.close();
        threw = false;
        try {
            twice.close();
        } catch (IllegalStateException expected) {
            threw = true;
        }
        check(threw, "closing a confined arena twice must raise IllegalStateException");
    }

    /**
     * {@code MemorySegment} read/write round trips across every width, plus the
     * bounds check.
     *
     * <p>Every width is asserted because the accessors are implemented one per
     * carrier and a fix applied to three of them is indistinguishable here from
     * a fix applied to all -- the same reasoning that made {@code RJdkHandles}
     * assert {@code asCollector} once per array carrier.
     */
    static void segmentRoundTrip() throws Throwable {
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment seg = arena.allocate(64);
            check(seg.byteSize() == 64L, "allocate(64) must have byteSize 64");
            check(seg.address() != 0L, "a native segment must have a non-zero address");

            seg.set(ValueLayout.JAVA_BYTE, 0, (byte) -7);
            seg.set(ValueLayout.JAVA_SHORT, 2, (short) -300);
            seg.set(ValueLayout.JAVA_CHAR, 4, 'Q');
            seg.set(ValueLayout.JAVA_INT, 8, -123456);
            seg.set(ValueLayout.JAVA_LONG, 16, -1234567890123L);
            seg.set(ValueLayout.JAVA_FLOAT, 24, 2.5f);
            seg.set(ValueLayout.JAVA_DOUBLE, 32, -6.25d);
            seg.set(ValueLayout.JAVA_BOOLEAN, 40, true);

            // Negative values throughout: a VM that dropped the sign, or that
            // widened a short as unsigned, answers a plausible number here and a
            // wrong one. 'Q' is 0x51 so a char read as a byte would also differ.
            check(seg.get(ValueLayout.JAVA_BYTE, 0) == (byte) -7, "byte round trip");
            check(seg.get(ValueLayout.JAVA_SHORT, 2) == (short) -300, "short round trip");
            check(seg.get(ValueLayout.JAVA_CHAR, 4) == 'Q', "char round trip");
            check(seg.get(ValueLayout.JAVA_INT, 8) == -123456, "int round trip");
            check(seg.get(ValueLayout.JAVA_LONG, 16) == -1234567890123L, "long round trip");
            check(seg.get(ValueLayout.JAVA_FLOAT, 24) == 2.5f, "float round trip");
            check(seg.get(ValueLayout.JAVA_DOUBLE, 32) == -6.25d, "double round trip");
            check(seg.get(ValueLayout.JAVA_BOOLEAN, 40), "boolean round trip");

            // A neighbouring slot must be untouched: this is what separates a
            // correctly SIZED write from one that scribbled the whole segment.
            check(seg.get(ValueLayout.JAVA_BYTE, 1) == 0,
                    "a 1-byte write must not disturb the next byte");

            // Indexed accessors scale by the layout's byte size, so index 3 of a
            // JAVA_INT view is byte offset 12, not 3.
            seg.setAtIndex(ValueLayout.JAVA_INT, 3, 0x0BADF00D);
            check(seg.getAtIndex(ValueLayout.JAVA_INT, 3) == 0x0BADF00D,
                    "indexed int round trip");
            check(seg.get(ValueLayout.JAVA_INT, 12) == 0x0BADF00D,
                    "setAtIndex(JAVA_INT, 3) must write byte offset 12");

            // asSlice re-bases: the slice's offset 0 is the parent's offset 8.
            MemorySegment slice = seg.asSlice(8, 8);
            check(slice.byteSize() == 8L, "asSlice(8, 8) must have byteSize 8");
            check(slice.get(ValueLayout.JAVA_INT, 0) == -123456,
                    "a slice must read the parent's bytes at its own offset 0");

            // Strings, both directions.
            MemorySegment str = arena.allocateFrom("hello");
            check(str.byteSize() == 6L, "allocateFrom must include the NUL terminator");
            check("hello".equals(str.getString(0)), "string round trip");

            // The bounds check, asserted by type. The positive control is every
            // successful access above.
            boolean threw = false;
            try {
                int oob = seg.get(ValueLayout.JAVA_INT, 62);
                check(oob == -1, "unreachable: read " + oob + " past the segment end");
            } catch (IndexOutOfBoundsException expected) {
                threw = true;
            }
            check(threw, "an out-of-bounds segment read must raise IndexOutOfBoundsException");

            threw = false;
            try {
                seg.get(ValueLayout.JAVA_INT, -4);
            } catch (IndexOutOfBoundsException expected) {
                threw = true;
            }
            check(threw, "a negative segment offset must raise IndexOutOfBoundsException");
        }

        check(MemorySegment.NULL.address() == 0L, "MemorySegment.NULL must have address 0");
        System.out.println("CK RJdkForeign segment ok");
    }

    /**
     * {@code VarHandle} access derived from a {@code ValueLayout}.
     *
     * <p>A layout-derived var handle takes {@code (MemorySegment, long)}
     * coordinates. The coordinate types are asserted as well as the values,
     * because a handle with the wrong coordinates can still answer correctly
     * for the one call shape a value-only test happens to write.
     */
    static void layoutVarHandles() throws Throwable {
        VarHandle vhInt = ValueLayout.JAVA_INT.varHandle();
        check(vhInt.varType() == int.class, "a JAVA_INT var handle must have varType int");
        check(vhInt.coordinateTypes().equals(List.of(MemorySegment.class, long.class)),
                "a layout var handle takes (MemorySegment, long), got "
                        + vhInt.coordinateTypes());

        try (Arena arena = Arena.ofConfined()) {
            MemorySegment seg = arena.allocate(32);
            vhInt.set(seg, 0L, 11);
            vhInt.set(seg, 4L, 22);
            check((int) vhInt.get(seg, 0L) == 11, "var handle set/get at offset 0");
            check((int) vhInt.get(seg, 4L) == 22, "var handle set/get at offset 4");
            // Cross-check against the layout accessor: the two views must agree,
            // which a var handle wired to the wrong offset cannot manage.
            check(seg.get(ValueLayout.JAVA_INT, 4) == 22,
                    "a var handle write must be visible through the layout accessor");

            VarHandle vhLong = ValueLayout.JAVA_LONG.varHandle();
            vhLong.set(seg, 8L, -9876543210L);
            check((long) vhLong.get(seg, 8L) == -9876543210L, "long var handle round trip");

            // A sequence-layout path element adds an INDEX coordinate on top of
            // the base offset, which is a different shape from the plain layout
            // handle above -- and it is the shape real FFM code writes.
            MemoryLayout seq = MemoryLayout.sequenceLayout(4, ValueLayout.JAVA_INT);
            VarHandle elem = seq.varHandle(MemoryLayout.PathElement.sequenceElement());
            MemorySegment arr = arena.allocate(seq);
            check(arr.byteSize() == 16L, "sequenceLayout(4, JAVA_INT) must be 16 bytes");
            for (int i = 0; i < 4; i++) {
                elem.set(arr, 0L, (long) i, (i + 1) * 100);
            }
            int sum = 0;
            for (int i = 0; i < 4; i++) {
                sum += (int) elem.get(arr, 0L, (long) i);
            }
            check(sum == 1000, "sequence-element var handle must address all four slots, got "
                    + sum);
            // Position, not just presence: element 2 is byte offset 8.
            check(arr.get(ValueLayout.JAVA_INT, 8) == 300,
                    "sequence element 2 must live at byte offset 8");

            boolean threw = false;
            try {
                int oob = (int) elem.get(arr, 0L, 9L);
                check(oob == -1, "unreachable: read " + oob + " past the sequence end");
            } catch (IndexOutOfBoundsException expected) {
                threw = true;
            }
            check(threw, "an out-of-bounds sequence index must raise IndexOutOfBoundsException");
        }

        // A layout var handle must refuse a misaligned access, not perform it.
        // Native memory: `arena.allocate(16, 16)` guarantees a 16-aligned
        // base; `.asSlice(4)` is then 4-aligned but never 8-aligned,
        // deterministically -- a heap array's alignment is fixed by its
        // element type, so it cannot be tuned to an arbitrary misalignment
        // the way a native slice can. JAVA_LONG needs 8.
        // ffm-varhandle-address-carrier-and-missing-alignment-check-20260921-RETIRED-20260921.md
        VarHandle vhLongMisaligned = ValueLayout.JAVA_LONG.varHandle();
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment base = arena.allocate(16, 16);
            MemorySegment misaligned = base.asSlice(4, 8);
            boolean misalignedSetThrew = false;
            try {
                vhLongMisaligned.set(misaligned, 0L, 0x6162636465666768L);
            } catch (IllegalArgumentException expected) {
                misalignedSetThrew = true;
            }
            check(misalignedSetThrew, "a var handle set whose layout alignment exceeds the "
                    + "segment's must be refused");
            boolean misalignedGetThrew = false;
            try {
                long unreachable = (long) vhLongMisaligned.get(misaligned, 0L);
                check(false,
                        "unreachable: read " + unreachable + " through a misaligned var handle");
            } catch (IllegalArgumentException expected) {
                misalignedGetThrew = true;
            }
            check(misalignedGetThrew, "a var handle get whose layout alignment exceeds the "
                    + "segment's must be refused");

            // Negative control: the same layout on 8-aligned native memory
            // must NOT be refused -- otherwise the checks above would be
            // satisfied by a var handle that refuses everything.
            MemorySegment aligned = arena.allocate(8, 8);
            vhLongMisaligned.set(aligned, 0L, 42L);
            check((long) vhLongMisaligned.get(aligned, 0L) == 42L,
                    "an aligned var handle access on 8-aligned native memory must succeed");

            // The enforced alignment is the layout `varHandle()` was called
            // ON, not the resolved leaf member's own alignment. `u0`
            // (JAVA_INT, its own alignment 4) sits in a union whose OTHER
            // member is JAVA_LONG, so the union's byteAlignment() is 8 --
            // MEASURED on Temurin 25.0.4: `union.varHandle(groupElement(
            // "u0"))` on a 4-alignable segment throws even though a bare
            // JAVA_INT.varHandle() on the same segment does not (case B in
            // the record's probe).
            MemoryLayout union = MemoryLayout.unionLayout(
                    ValueLayout.JAVA_INT.withName("u0"), ValueLayout.JAVA_LONG.withName("u1"));
            check(union.byteAlignment() == 8L, "union of int+long must have byteAlignment 8");
            VarHandle unionMemberVh = union.varHandle(MemoryLayout.PathElement.groupElement("u0"));
            boolean unionMisalignedThrew = false;
            try {
                unionMemberVh.set(misaligned, 0L, 0x61626364);
            } catch (IllegalArgumentException expected) {
                unionMisalignedThrew = true;
            }
            check(unionMisalignedThrew, "a group-member var handle must be refused when the "
                    + "ENCLOSING layout's alignment (not just the member's own) exceeds what the "
                    + "segment offers");
            // Positive control for the SAME union member handle: aligned
            // memory must succeed, and read back what was written -- so the
            // refusal above cannot be a handle that always throws.
            unionMemberVh.set(aligned, 0L, 0x61626364);
            check((int) unionMemberVh.get(aligned, 0L) == 0x61626364,
                    "an aligned group-member var handle access must round-trip");
        }

        // A layout var handle must work on a HEAP-backed segment too, not
        // only a native (Arena-allocated) one. `MemorySegment.ofArray(...)`
        // has no native pointer -- see the FFM record above, §2's own
        // fix-and-measure block: this used to be refused outright with a
        // spurious IndexOutOfBoundsException, for EVERY heap segment.
        MemorySegment intArraySeg = MemorySegment.ofArray(new int[4]);
        vhInt.set(intArraySeg, 0L, 11);
        vhInt.set(intArraySeg, 4L, 22);
        check((int) vhInt.get(intArraySeg, 0L) == 11, "heap var handle set/get at offset 0");
        check((int) vhInt.get(intArraySeg, 4L) == 22, "heap var handle set/get at offset 4");
        check(intArraySeg.get(ValueLayout.JAVA_INT, 4) == 22,
                "a heap var handle write must be visible through the layout accessor");

        // A carrier wider than the backing array's own element type, crossing
        // element boundaries -- exercises `heap_bytes_read`/`heap_bytes_write`'s
        // general (non-element-aligned) path, not just its byte[]/int[]
        // fast paths. `JAVA_INT_UNALIGNED`: `byte[]` only offers 1-byte
        // alignment, so a plain (4-aligned) `JAVA_INT` would be refused here,
        // not because of a bug but correctly, the same rule §2 added.
        MemorySegment byteArraySeg = MemorySegment.ofArray(new byte[8]);
        VarHandle vhIntUnaligned = ValueLayout.JAVA_INT_UNALIGNED.varHandle();
        vhIntUnaligned.set(byteArraySeg, 0L, 0x01020304);
        check(byteArraySeg.get(ValueLayout.JAVA_BYTE, 0) == 4
                        && byteArraySeg.get(ValueLayout.JAVA_BYTE, 3) == 1,
                "a heap var handle write must land little-endian, byte-addressable");
        check((int) vhIntUnaligned.get(byteArraySeg, 0L) == 0x01020304,
                "a heap var handle wider than the backing element must round-trip");

        // A `long[]`-backed segment: the OTHER heap element width, and a
        // carrier wider than a native `int` word.
        MemorySegment longArraySeg = MemorySegment.ofArray(new long[2]);
        vhLongMisaligned.set(longArraySeg, 8L, -9876543210L);
        check((long) vhLongMisaligned.get(longArraySeg, 8L) == -9876543210L,
                "heap long[]-backed var handle round trip");

        // Big-endian carrier on a heap segment -- the heap read/write path
        // has its own endianness handling (byte-swap around
        // `heap_bytes_read`/`write`'s always-little-endian core), separate
        // from the native path's, and must agree with it.
        MemorySegment beByteSeg = MemorySegment.ofArray(new byte[8]);
        VarHandle vhIntUnalignedBe =
                ValueLayout.JAVA_INT_UNALIGNED.withOrder(java.nio.ByteOrder.BIG_ENDIAN).varHandle();
        vhIntUnalignedBe.set(beByteSeg, 0L, 0x01020304);
        check(beByteSeg.get(ValueLayout.JAVA_BYTE, 0) == 1,
                "a big-endian heap var handle write must place the MSB first");
        check((int) vhIntUnalignedBe.get(beByteSeg, 0L) == 0x01020304,
                "a big-endian heap var handle must round-trip");

        // Out-of-bounds heap access must refuse the same way an out-of-bounds
        // native access does.
        boolean heapOobThrew = false;
        try {
            int unreachable = (int) vhInt.get(intArraySeg, 16L); // offset 16, width 4, size 16
            check(false, "unreachable: read " + unreachable + " past the heap segment end");
        } catch (IndexOutOfBoundsException expected) {
            heapOobThrew = true;
        }
        check(heapOobThrew, "an out-of-bounds heap var handle access must raise "
                + "IndexOutOfBoundsException");

        // Misaligned heap access must be refused too -- the ORIGINAL failing
        // case this whole record started from (`L4FfmLayoutSweep`'s
        // `addr.varHandle` / `group.union.varHandle.roundtrip` diffs), now
        // reachable at all because heap addressing works.
        boolean heapMisalignedThrew = false;
        try {
            vhLongMisaligned.set(MemorySegment.ofArray(new int[2]), 0L, 42L);
        } catch (IllegalArgumentException expected) {
            heapMisalignedThrew = true;
        }
        check(heapMisalignedThrew, "a misaligned heap var handle access must raise "
                + "IllegalArgumentException, not IndexOutOfBoundsException and not succeed");

        // `ADDRESS`'s VarHandle must report and move a REAL MemorySegment,
        // not a `long` -- see `docs/internal/jdk-only/
        // ffm-varhandle-address-carrier-and-missing-alignment-check-20260921-RETIRED-20260921.md`
        // §1. MEASURED on Temurin 25.0.4 (`AddressVhProbe`) for every
        // assertion below.
        VarHandle addrVh = ValueLayout.ADDRESS.varHandle();
        check(addrVh.varType() == MemorySegment.class,
                "an ADDRESS var handle's varType must be MemorySegment, not long");
        check(addrVh.coordinateTypes().equals(List.of(MemorySegment.class, long.class)),
                "an ADDRESS var handle's coordinates must be (MemorySegment, long)");

        try (Arena arena = Arena.ofConfined()) {
            MemorySegment seg = arena.allocate(16);

            // Reading a raw pointer-shaped long through the ADDRESS var
            // handle must mint a MemorySegment wrapping that address, not
            // hand back the bits as a boxed Long.
            long rawPtr = 0x1122334455667788L;
            seg.set(ValueLayout.JAVA_LONG, 0, rawPtr);
            Object got = addrVh.get(seg, 0L);
            check(got instanceof MemorySegment,
                    "ADDRESS var handle get() must return a MemorySegment, got "
                            + (got == null ? "null" : got.getClass()));
            MemorySegment gotSeg = (MemorySegment) got;
            check(gotSeg.address() == rawPtr,
                    "the minted segment's address must be the raw pointer value, got "
                            + Long.toHexString(gotSeg.address()));
            check(gotSeg.byteSize() == 0L,
                    "an ADDRESS-carrier read has no declared size, so byteSize() must be 0");

            // Writing a MemorySegment VALUE through the ADDRESS var handle
            // must store its address, not fail to move anything at all.
            MemorySegment other = arena.allocate(8);
            addrVh.set(seg, 0L, other);
            check(seg.get(ValueLayout.JAVA_LONG, 0) == other.address(),
                    "an ADDRESS var handle set(segment) must store the segment's own address");

            // Round trip entirely through the ADDRESS var handle.
            addrVh.set(seg, 8L, other);
            MemorySegment roundTrip = (MemorySegment) addrVh.get(seg, 8L);
            check(roundTrip.address() == other.address(),
                    "an ADDRESS var handle round trip must preserve the address");

            // MemorySegment.NULL is a real, non-null object whose address()
            // is 0 -- writing it must round-trip to a segment that equals
            // MemorySegment.NULL, not throw.
            addrVh.set(seg, 0L, MemorySegment.NULL);
            check(seg.get(ValueLayout.JAVA_LONG, 0) == 0L,
                    "writing MemorySegment.NULL through an ADDRESS var handle must store 0");
            MemorySegment nullGot = (MemorySegment) addrVh.get(seg, 0L);
            check(nullGot.equals(MemorySegment.NULL),
                    "reading address 0 through an ADDRESS var handle must equal MemorySegment.NULL");

            // A literal Java `null` is NOT MemorySegment.NULL and must NPE --
            // the null-coordinate rule every other reference-typed VarHandle
            // coordinate already gets.
            boolean threwNpe = false;
            try {
                addrVh.set(seg, 0L, (MemorySegment) null);
            } catch (NullPointerException expected) {
                threwNpe = true;
            }
            check(threwNpe,
                    "an ADDRESS var handle set(null) must raise NullPointerException");

            // A struct member of ADDRESS type -- the group-element path
            // through the same fix, not just the bare-layout path above.
            MemoryLayout struct = MemoryLayout.structLayout(
                    ValueLayout.ADDRESS.withName("ptr"),
                    ValueLayout.JAVA_INT.withName("tag"));
            VarHandle ptrVh = struct.varHandle(MemoryLayout.PathElement.groupElement("ptr"));
            check(ptrVh.varType() == MemorySegment.class,
                    "a group-member ADDRESS var handle's varType must be MemorySegment too");
            MemorySegment structSeg = arena.allocate(struct);
            ptrVh.set(structSeg, 0L, other);
            MemorySegment ptrGot = (MemorySegment) ptrVh.get(structSeg, 0L);
            check(ptrGot.address() == other.address(),
                    "a group-member ADDRESS var handle must round-trip the address");
        }

        // ADDRESS var handle over a HEAP segment -- the same address-carrier
        // fix must apply on both addressing paths, not just the native one.
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment heapSeg = MemorySegment.ofArray(new long[2]);
            MemorySegment other2 = arena.allocate(8);
            addrVh.set(heapSeg, 0L, other2);
            MemorySegment heapGot = (MemorySegment) addrVh.get(heapSeg, 0L);
            check(heapGot.address() == other2.address(),
                    "a heap-backed ADDRESS var handle must round-trip the address");
        }

        System.out.println("CK RJdkForeign varhandle ok");
    }

    /**
     * Layout metadata, which the downcall descriptor is built out of.
     *
     * <p>These are cheap and they are the substrate every step above stands on:
     * a wrong {@code byteSize} makes every offset in this file wrong in a way
     * that would otherwise be reported as an accessor defect.
     */
    static void layouts() throws Throwable {
        check(ValueLayout.JAVA_BYTE.byteSize() == 1L, "JAVA_BYTE byteSize");
        check(ValueLayout.JAVA_SHORT.byteSize() == 2L, "JAVA_SHORT byteSize");
        check(ValueLayout.JAVA_CHAR.byteSize() == 2L, "JAVA_CHAR byteSize");
        check(ValueLayout.JAVA_INT.byteSize() == 4L, "JAVA_INT byteSize");
        check(ValueLayout.JAVA_LONG.byteSize() == 8L, "JAVA_LONG byteSize");
        check(ValueLayout.JAVA_FLOAT.byteSize() == 4L, "JAVA_FLOAT byteSize");
        check(ValueLayout.JAVA_DOUBLE.byteSize() == 8L, "JAVA_DOUBLE byteSize");
        check(ValueLayout.JAVA_INT.carrier() == int.class, "JAVA_INT carrier");
        check(ValueLayout.ADDRESS.carrier() == MemorySegment.class, "ADDRESS carrier");

        MemoryLayout struct = MemoryLayout.structLayout(
                ValueLayout.JAVA_INT.withName("a"),
                ValueLayout.JAVA_INT.withName("b"),
                ValueLayout.JAVA_LONG.withName("c"));
        check(struct.byteSize() == 16L, "structLayout(int,int,long) must be 16 bytes, got "
                + struct.byteSize());
        check(struct.byteOffset(MemoryLayout.PathElement.groupElement("c")) == 8L,
                "the long member must sit at byte offset 8");

        FunctionDescriptor fd = FunctionDescriptor.of(ValueLayout.JAVA_LONG, ValueLayout.ADDRESS);
        check(fd.returnLayout().isPresent(), "a value-returning descriptor must have a return layout");
        check(fd.returnLayout().get().equals(ValueLayout.JAVA_LONG),
                "the return layout must be the one supplied");
        check(fd.argumentLayouts().size() == 1, "the descriptor must carry one argument layout");
        check(fd.argumentLayouts().get(0).equals(ValueLayout.ADDRESS),
                "the argument layout must be the one supplied");
        FunctionDescriptor voidFd = FunctionDescriptor.ofVoid(ValueLayout.JAVA_INT);
        check(voidFd.returnLayout().isEmpty(), "a void descriptor must have no return layout");
        System.out.println("CK RJdkForeign layouts struct=" + struct.byteSize());
    }

    public static void main(String[] args) throws Throwable {
        step("layouts", RJdkForeign::layouts);
        step("downcall", RJdkForeign::downcall);
        step("downcallInt", RJdkForeign::downcallInt);
        step("heapSegmentDowncallRefusal", RJdkForeign::heapSegmentDowncallRefusal);
        step("handleIsARealMethodHandle", RJdkForeign::handleIsARealMethodHandle);
        step("arenaScoping", RJdkForeign::arenaScoping);
        step("segmentRoundTrip", RJdkForeign::segmentRoundTrip);
        step("layoutVarHandles", RJdkForeign::layoutVarHandles);
        System.out.println("CK RJdkForeign steps=" + steps);
        System.out.println("CK RJdkForeign checks=" + checks);
        if (!failures.isEmpty()) {
            throw new AssertionError(failures.size() + " of " + steps
                    + " steps failed: " + failures);
        }
        System.out.println("PASS RJdkForeign (" + checks + " checks, " + steps + " steps)");
    }
}
