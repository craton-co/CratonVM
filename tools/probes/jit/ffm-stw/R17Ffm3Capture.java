// SPDX-License-Identifier: Apache-2.0
// JIT round 17 wave 2 (lane ffm3): the capture-state segment checks of a
// captureCallState downcall, as HotSpot's SharedUtils.checkCaptureSegment
// makes them before the call (r17-ffm-capture-segment-null-and-alignment-
// checks-20261005): Objects.requireNonNull (a message-less NPE), a refusal of
// a segment equal to MemorySegment.NULL (IllegalArgumentException "Capture
// segment is NULL: <segment>"), then asSlice(0, captureStateLayout()):
// bounds (IndexOutOfBoundsException) before alignment
// (IllegalArgumentException "Target offset incompatible with alignment
// constraints").
//
//   java --enable-native-access=ALL-UNNAMED R17Ffm3Capture
//
// Every case runs three ways: through a compiled loop that served a good
// state segment 20000 times first ("hot"), through a fresh handle ("cold"),
// and again through the hot loop after the refusal ("again", proving the
// refusal left the site usable). One line per case and way; no timings.
// The IndexOutOfBoundsException message is cut after the segment text
// (the address differs per run) and a heap refusal to "Heap segment not
// allowed" (an identity hash); every other message is printed whole.
// "heap-ints" records HotSpot's answer for a 4-aligned heap state segment
// (not predicted by this lane; CratonVM returns normally there).
//
// Arms (CratonVM): default (CRATONVM_FFM_CAPTURE_SEGMENT_NULL_ALIGN_CHECK on)
// must match HotSpot; =0 is the positive control (null -> IllegalStateException,
// NULL and misaligned -> no exception).
import java.lang.foreign.*;
import java.lang.invoke.MethodHandle;

public class R17Ffm3Capture {
    static final Linker LINKER = Linker.nativeLinker();
    static final SymbolLookup STD = LINKER.defaultLookup();
    static final FunctionDescriptor LL = FunctionDescriptor.of(ValueLayout.JAVA_LONG, ValueLayout.JAVA_LONG);
    static boolean bad;

    static MethodHandle labs() {
        return LINKER.downcallHandle(STD.find("llabs").orElseThrow(), LL,
                Linker.Option.captureCallState("errno"));
    }

    static long loop(MethodHandle h, MemorySegment state, int n) throws Throwable {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            long v = (long) h.invokeExact(state, (long) -i);
            if (v != i) {
                bad = true;
            }
            sum += v;
        }
        return sum;
    }

    static String describe(Throwable t) {
        String m = t.getMessage();
        if (m != null && m.startsWith("Heap segment not allowed")) {
            m = "Heap segment not allowed";
        }
        if (m != null && m.startsWith("Out of bound access") && m.contains("; new offset")) {
            m = m.substring(m.indexOf("; new offset") + 2);
        }
        return t.getClass().getName() + ": " + m;
    }

    interface Body {
        long run() throws Throwable;
    }

    static void fact(String label, Body body) {
        try {
            System.out.println(label + ": " + body.run());
        } catch (Throwable t) {
            System.out.println(label + ": threw " + describe(t));
        }
    }

    public static void main(String[] args) throws Throwable {
        Arena arena = Arena.ofConfined();
        MethodHandle hot = labs();
        MemorySegment good = arena.allocate(Linker.Option.captureStateLayout());
        fact("warm", () -> loop(hot, good, 20_000));

        MemorySegment wide = arena.allocate(32, 16);
        MemorySegment[] states = {
            null,
            MemorySegment.NULL,
            MemorySegment.NULL.reinterpret(16),
            wide.asSlice(1),
            wide.asSlice(2, 8),
            wide.asSlice(1, 3),
            wide.asSlice(4),
            MemorySegment.ofArray(new byte[16]),
            MemorySegment.ofArray(new int[4]),
        };
        String[] names = {
            "null",
            "NULL",
            "NULL-sized",
            "offset-one",
            "offset-two",
            "offset-one-small",
            "offset-four",
            "heap-bytes",
            "heap-ints",
        };
        for (int k = 0; k < states.length; k++) {
            MemorySegment s = states[k];
            fact(names[k] + " hot", () -> loop(hot, s, 3));
            fact(names[k] + " cold", () -> loop(labs(), s, 3));
            fact(names[k] + " again", () -> loop(hot, good, 3));
        }
        System.out.println("layout: " + Linker.Option.captureStateLayout().byteAlignment());
        System.out.println(bad ? "FAIL" : "PASS");
        arena.close();
    }
}
