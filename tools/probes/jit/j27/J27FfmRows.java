// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round j27 wave 4 (lane FFMROWS): the FFM edges the backlog rows of
// docs/feature-designs/jit-ir-optimizations.md ("FFM / panama", "FFM downcall MH")
// depend on, interpreted and from compiled code. One `fact <name> <value>` line per answer; diff the
// whole stdout against HotSpot 25 (expected lines: the live-rows page, lane FFMROWS block).
//   javac -d tools/probes/out tools/probes/jit/j27/J27FfmRows.java
//   java --enable-native-access=ALL-UNNAMED -cp tools/probes/out J27FfmRows
//   cratonvm --enable-native-access=ALL-UNNAMED -cp tools/probes/out J27FfmRows   (and --compatible)
// Assumes a little-endian host (x86-64). No timing lines.
import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.Linker;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;
import java.nio.ByteOrder;

public class J27FfmRows {
    static final ValueLayout.OfInt BE_INT = ValueLayout.JAVA_INT.withOrder(ByteOrder.BIG_ENDIAN);
    static final int N = 1024;
    static final int WARM = 30_000;

    static void fact(String name, Object value) {
        System.out.println("fact " + name + " " + value);
    }

    /** Class name, and the message up to its first ':' (addresses after it vary per run). */
    static String ex(Throwable t) {
        String m = t.getMessage();
        if (m != null && m.indexOf(':') >= 0) {
            m = m.substring(0, m.indexOf(':'));
        }
        return t.getClass().getName() + (m == null ? "" : " " + m);
    }

    static int sumInts(MemorySegment s, int n) {
        int h = 0;
        for (int k = 0; k < n; k++) {
            h += s.getAtIndex(ValueLayout.JAVA_INT, k);
        }
        return h;
    }

    static int sumBe(MemorySegment s, int n) {
        int h = 0;
        for (int k = 0; k < n; k++) {
            h += s.getAtIndex(BE_INT, k);
        }
        return h;
    }

    static void fill(MemorySegment s, int n) {
        for (int k = 0; k < n; k++) {
            s.setAtIndex(ValueLayout.JAVA_INT, k, 31 * k + 7);
        }
    }

    static int getInt(MemorySegment s, long off) {
        return s.get(ValueLayout.JAVA_INT, off);
    }

    static void setInt(MemorySegment s, long off, int v) {
        s.set(ValueLayout.JAVA_INT, off, v);
    }

    static String attempt(Runnable r) {
        try {
            r.run();
            return "ok";
        } catch (RuntimeException e) {
            return ex(e);
        }
    }

    /** A hot loop over a native and a heap segment, in native and big-endian order. */
    static void hotLoops(Arena arena) {
        MemorySegment nat = arena.allocate(4L * N, 8);
        MemorySegment heap = MemorySegment.ofArray(new int[N]);
        boolean stable = true;
        int nat0 = 0, heap0 = 0, natBe = 0, heapBe = 0;
        for (int i = 0; i < WARM; i++) {
            fill(nat, N);
            fill(heap, N);
            int a = sumInts(nat, N), b = sumInts(heap, N), c = sumBe(nat, N), d = sumBe(heap, N);
            if (i == 0) {
                nat0 = a; heap0 = b; natBe = c; heapBe = d;
            } else if (a != nat0 || b != heap0 || c != natBe || d != heapBe) {
                stable = false;
            }
        }
        fact("hot-native-sum", nat0);
        fact("hot-heap-sum", heap0);
        fact("hot-native-be-sum", natBe);
        fact("hot-heap-be-sum", heapBe);
        fact("hot-stable", stable);
    }

    /** Bounds, alignment and read-only answers of an accessor the loop above compiled. */
    static void compiledEdges(Arena arena) {
        MemorySegment nat = arena.allocate(64, 8);
        long sink = 0;
        for (int i = 0; i < WARM; i++) {
            setInt(nat, (i & 7) * 4L, i);
            sink += getInt(nat, (i & 7) * 4L);
        }
        fact("compiled-warm", sink != 0);
        fact("compiled-oob", attempt(() -> getInt(nat, 64)));
        fact("compiled-negative", attempt(() -> getInt(nat, -4)));
        fact("compiled-misaligned-native", attempt(() -> getInt(nat, 1)));
        MemorySegment bytes = MemorySegment.ofArray(new byte[16]);
        fact("compiled-misaligned-heap-bytes", attempt(() -> getInt(bytes, 0)));
        MemorySegment ints = MemorySegment.ofArray(new int[4]);
        fact("compiled-heap-int", attempt(() -> setInt(ints, 4, 42)) + " " + getInt(ints, 4));
        fact("compiled-read-only", attempt(() -> setInt(nat.asReadOnly(), 0, 1)));
    }

    /** A closed confined arena, and a confined segment from another thread, in compiled code. */
    static void compiledLiveness() throws InterruptedException {
        Arena arena = Arena.ofConfined();
        MemorySegment s = arena.allocate(4L * 64, 4);
        fill(s, 64);
        int sum = 0;
        for (int i = 0; i < WARM; i++) {
            sum = sumInts(s, 64);
        }
        fact("liveness-warm", sum);
        String[] other = new String[1];
        Thread t = new Thread(() -> other[0] = attempt(() -> sumInts(s, 64)), "not-the-owner");
        t.start();
        t.join();
        fact("compiled-wrong-thread", other[0]);
        arena.close();
        fact("compiled-closed", attempt(() -> sumInts(s, 64)));
        fact("compiled-closed-get", attempt(() -> getInt(s, 0)));
    }

    static void interpretedEdges() throws InterruptedException {
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment s = arena.allocate(16, 8);
            s.set(BE_INT, 0, 0x01020304);
            fact("byte-order-first-byte", s.get(ValueLayout.JAVA_BYTE, 0));
            fact("byte-order-le-read", s.get(ValueLayout.JAVA_INT.withOrder(ByteOrder.LITTLE_ENDIAN), 0));
            fact("unaligned-layout", attempt(() -> s.get(ValueLayout.JAVA_INT_UNALIGNED, 1)));
            fact("bad-alignment-layout", attempt(() -> ValueLayout.JAVA_INT.withByteAlignment(3)));
            fact("bad-alignment-allocate", attempt(() -> arena.allocate(8, 3)));
            String[] other = new String[1];
            Thread t = new Thread(() -> other[0] = attempt(() -> s.get(ValueLayout.JAVA_INT, 0)));
            t.start();
            t.join();
            fact("wrong-thread", other[0]);
            MemorySegment heap = MemorySegment.ofArray(new int[2]);
            fact("heap-is-native", heap.isNative());
            fact("heap-address", heap.address());
            fact("heap-base-present", heap.heapBase().isPresent());
            fact("native-is-native", s.isNative());
            fact("native-heap-base-present", s.heapBase().isPresent());
        }
        Arena closed = Arena.ofConfined();
        MemorySegment c = closed.allocate(8, 8);
        closed.close();
        fact("closed", attempt(() -> c.get(ValueLayout.JAVA_INT, 0)));
        fact("closed-alive", c.scope().isAlive());
    }

    /** FFM417-4, R16B-182, R16B-183: downcall answers. */
    static void downcalls() throws Throwable {
        Linker linker = Linker.nativeLinker();
        MemorySegment strlenAddr = linker.defaultLookup().find("strlen").orElseThrow();
        FunctionDescriptor fd = FunctionDescriptor.of(ValueLayout.JAVA_LONG, ValueLayout.ADDRESS);
        MethodHandle strlen = linker.downcallHandle(strlenAddr, fd);
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment hello = arena.allocateFrom("hello");
            fact("strlen", (long) strlen.invokeExact(hello));
            // R16B-182: an asType copy of the downcall handle keeps its return conversion.
            MethodHandle boxed = strlen.asType(MethodType.methodType(Object.class, MemorySegment.class));
            Object r = boxed.invoke(hello);
            fact("downcall-astype", r + " " + r.getClass().getName());
            // R16B-183: a null argument through invokeExact.
            String nullArg;
            try {
                long n = (long) strlen.invokeExact((MemorySegment) null);
                nullArg = "ok " + n;
            } catch (RuntimeException e) {
                nullArg = e.getClass().getName() + " " + e.getMessage();
            }
            fact("downcall-null-arg", nullArg);
            // Heap segment to a plain handle: HotSpot refuses every heap segment.
            fact("downcall-heap-int-plain", attempt(() -> {
                try {
                    long n = (long) strlen.invokeExact(MemorySegment.ofArray(new int[] {0x6968, 0}));
                } catch (Throwable t) {
                    throw t instanceof RuntimeException re ? re : new RuntimeException(t);
                }
            }));
            // FFM417-4: critical(true) + captureCallState with a heap capture segment.
            String linked;
            MethodHandle cap = null;
            try {
                cap = linker.downcallHandle(strlenAddr, fd,
                        Linker.Option.critical(true), Linker.Option.captureCallState("errno"));
                linked = "linked";
            } catch (RuntimeException e) {
                linked = ex(e);
            }
            fact("capture-critical-link", linked);
            if (cap != null) {
                long words = (Linker.Option.captureStateLayout().byteSize() + 3) / 4;
                MemorySegment heapState = MemorySegment.ofArray(new int[(int) words]);
                MethodHandle h = cap;
                fact("capture-critical-heap-state", attempt(() -> {
                    try {
                        long n = (long) h.invokeExact(heapState, hello);
                        if (n != 5) {
                            throw new IllegalStateException("strlen " + n);
                        }
                    } catch (Throwable t) {
                        throw t instanceof RuntimeException re ? re : new RuntimeException(t);
                    }
                }));
            }
        }
    }

    /** Class name and full message, the identity hash after '@' stripped (it varies per run). */
    static String full(Throwable t) {
        String m = t.getMessage();
        return t.getClass().getName() + " " + (m == null ? "null" : m.replaceAll("@[0-9a-f]+", "@<hash>"));
    }

    static String attemptFull(Runnable r) {
        try {
            r.run();
            return "ok";
        } catch (RuntimeException e) {
            return full(e);
        }
    }

    /**
     * j27 wave 4 (lane FFMMSG): every `SharedUtils.checkNative` refusal says
     * `Heap segment not allowed: ` + the segment's own `toString()`, whose `heapBase` clause is
     * absent on a read-only segment and whose address is the offset into the array.
     */
    static void heapRefusals() throws Throwable {
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment nat = arena.allocate(16, 8);
            fact("refusal-set-address", attemptFull(
                    () -> nat.set(ValueLayout.ADDRESS, 0, MemorySegment.ofArray(new byte[4]))));
            VarHandle vh = ValueLayout.ADDRESS.varHandle();
            fact("refusal-vh-address", attemptFull(
                    () -> vh.set(nat, 0L, MemorySegment.ofArray(new int[2]))));
            MemorySegment ro = MemorySegment.ofArray(new int[4]).asSlice(4, 8).asReadOnly();
            fact("refusal-set-address-read-only", attemptFull(
                    () -> nat.set(ValueLayout.ADDRESS, 0, ro)));
            fact("refusal-stored-word", nat.get(ValueLayout.JAVA_LONG, 0));
            Linker linker = Linker.nativeLinker();
            MethodHandle strlen = linker.downcallHandle(
                    linker.defaultLookup().find("strlen").orElseThrow(),
                    FunctionDescriptor.of(ValueLayout.JAVA_LONG, ValueLayout.ADDRESS));
            MemorySegment slice = MemorySegment.ofArray(new byte[] {0x68, 0}).asSlice(1);
            fact("refusal-downcall-slice", attemptFull(() -> {
                try {
                    long n = (long) strlen.invokeExact(slice);
                } catch (Throwable t) {
                    throw t instanceof RuntimeException re ? re : new RuntimeException(t);
                }
            }));
        }
    }

    /**
     * j27 wave 4 (lane FFMMSG): `MemorySegment.toString()` and the refusals that embed it
     * (`outOfBoundException`, `checkEnclosingLayout`). A native address varies per run, so the
     * native row prints `<addr>` in its place and a second row checks it is `address()`.
     */
    static void segmentTexts() {
        MemorySegment bytes = MemorySegment.ofArray(new byte[16]);
        fact("tostring-heap", full(bytes.asSlice(3, 4).toString()));
        fact("tostring-heap-read-only", bytes.asSlice(3, 4).asReadOnly());
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment nat = arena.allocate(16, 8);
            String hex = "0x" + Long.toHexString(nat.address());
            fact("tostring-native", nat.toString().replace(hex, "0x<addr>"));
            fact("tostring-native-address", nat.toString().contains(hex + ","));
        }
        fact("slice-oob-text", attemptFull(() -> bytes.asSlice(17, 0)));
        MemorySegment eight = MemorySegment.ofArray(new byte[8]);
        fact("get-oob-text", attemptFull(() -> eight.get(ValueLayout.JAVA_INT_UNALIGNED, 6)));
        fact("get-misaligned-text", attemptFull(() -> bytes.get(ValueLayout.JAVA_INT, 0)));
        fact("address-heap-slice", bytes.asSlice(3).address());
        fact("copy-oob-text", attemptFull(
                () -> MemorySegment.copy(bytes, ValueLayout.JAVA_BYTE, 10, new byte[16], 0, 8)));
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment nat = arena.allocate(16, 8);
            String hex = "0x" + Long.toHexString(nat.address());
            fact("native-oob-text", attemptFull(() -> nat.get(ValueLayout.JAVA_INT, 14))
                    .replace(hex, "0x<addr>"));
            fact("native-misaligned-text", attemptFull(() -> nat.get(ValueLayout.JAVA_INT, 2))
                    .replace(hex, "0x<addr>"));
            // j27 wave 6 (lane T1LOOPFFM): a member handle is checked against its ENCLOSING
            // layout (`checkEnclosingLayout(base, enclosing)`): the struct's size and alignment.
            java.lang.foreign.StructLayout pair = java.lang.foreign.MemoryLayout.structLayout(
                    ValueLayout.JAVA_INT.withName("a"), ValueLayout.JAVA_INT.withName("b"));
            VarHandle second = pair.varHandle(java.lang.foreign.MemoryLayout.PathElement.groupElement("b"));
            fact("member-oob-text", attemptFull(() -> {
                int v = (int) second.get(nat, 12L);
            }).replace(hex, "0x<addr>"));
            fact("member-misaligned-text", attemptFull(() -> {
                int v = (int) second.get(nat, 2L);
            }).replace(hex, "0x<addr>"));
            fact("member-read", (int) second.get(nat, 8L));
        }
        // j27 wave 6 (lane T1LOOPFFM): bounds and alignment are checked BEFORE the session,
        // so a closed segment's out-of-bounds or misaligned access is not `Already closed`.
        {
            Arena gone = Arena.ofConfined();
            MemorySegment closed = gone.allocate(16, 8);
            String hex = "0x" + Long.toHexString(closed.address());
            gone.close();
            VarHandle intHandle = ValueLayout.JAVA_INT.varHandle();
            fact("closed-misaligned-text", attemptFull(() -> closed.get(ValueLayout.JAVA_INT, 2))
                    .replace(hex, "0x<addr>"));
            fact("closed-oob-text", attemptFull(() -> closed.get(ValueLayout.JAVA_INT, 14))
                    .replace(hex, "0x<addr>"));
            fact("closed-aligned-text", attemptFull(() -> closed.get(ValueLayout.JAVA_INT, 0)));
            fact("closed-vh-misaligned-text", attemptFull(() -> {
                int v = (int) intHandle.get(closed, 2L);
            }).replace(hex, "0x<addr>"));
        }
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment four = arena.allocate(4, 1);
            four.fill((byte) 'a');
            String fourHex = "0x" + Long.toHexString(four.address());
            fact("getstring-unterminated", attemptFull(() -> four.getString(0))
                    .replace(fourHex, "0x<addr>"));
            fact("getstring-at-end", attemptFull(() -> four.getString(4))
                    .replace(fourHex, "0x<addr>"));
            fact("getstring-past-end", attemptFull(() -> four.getString(6))
                    .replace(fourHex, "0x<addr>"));
            int big = 3 << 19; // 1.5 MiB, past the old 1 MiB scan cap
            MemorySegment longString = arena.allocate(big + 1L, 1);
            longString.fill((byte) 'a');
            longString.set(ValueLayout.JAVA_BYTE, big, (byte) 0);
            fact("getstring-past-1mib", longString.getString(0).length());
        }
        byte[] heapBytes = {'h', 'i', 0, 'y', 'z'};
        MemorySegment heapString = MemorySegment.ofArray(heapBytes);
        fact("getstring-heap", heapString.getString(0) + " " + heapString.getString(1));
        fact("getstring-heap-unterminated", full(attemptFull(() -> heapString.getString(3))));
    }

    /** `full`'s hash normalisation for a plain string. */
    static String full(String s) {
        return s.replaceAll("@[0-9a-f]+", "@<hash>");
    }

    public static void main(String[] args) throws Throwable {
        interpretedEdges();
        try (Arena arena = Arena.ofConfined()) {
            hotLoops(arena);
            compiledEdges(arena);
        }
        compiledLiveness();
        downcalls();
        heapRefusals();
        segmentTexts();
    }
}
