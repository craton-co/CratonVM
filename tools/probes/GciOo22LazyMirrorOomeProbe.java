// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gci i22/oo ({@code gcg-g4oo-oome-ladder-review-findings} item 4): the first
 * {@code Object.getClass()} of an object whose class has no
 * {@code java.lang.Class} mirror yet, asked while the heap is full of GARBAGE
 * that nothing has collected yet. HotSpot creates every mirror at class load,
 * so {@code getClass()} allocates nothing and answers. CratonVM mints the
 * mirror lazily in the {@code getClass} native; before i22/oo Generational
 * threw a catchable {@code OutOfMemoryError} there with nothing collected (G1
 * raised it with no Free region left), because the native's fallible door
 * declined on ZGC only. After it, the native collects and asks again.
 *
 * <p>Each stage: the object is made first (its class loaded, initialized);
 * the heap is then filled with LIVE chains (big arrays until the first error,
 * small ones until the second), the chains are dropped by returning (garbage,
 * not yet collected), and {@code getClass()} is the next thing that runs.
 * Nothing is allocated between the drop and the call.
 *
 * <p>Modes (first argument):
 * <ul>
 *   <li>{@code user}: three classes of this file. CratonVM makes a class's
 *       mirror at initialization ({@code CRATONVM_GC_MIRROR_AT_INIT}, default
 *       ON), so the lazy mint happens only with
 *       {@code CRATONVM_GC_MIRROR_AT_INIT=0}: the deciding rows set it;</li>
 *   <li>{@code vmexc}: three VM-raised exceptions (arithmetic, negative array
 *       size, array store), whose classes the VM may link before it
 *       initializes them (no init-path mirror then); engagement not assured.</li>
 *   <li>{@code boot} (gci i26/oo2): {@code java.util.zip.Adler32},
 *       {@code java.util.zip.CRC32C},
 *       {@code java.util.concurrent.atomic.DoubleAdder} -- bootstrap classes
 *       the VM defines itself, so no {@code defineClass1} made their mirror
 *       at load (a class-path class under {@code --jdk-only} may have one
 *       from the Java app loader, which would make {@code user} never mint;
 *       the likely reason vi22-oo M2 was NOT SHOWN).</li>
 * </ul>
 * gci i26/oo2: the fill has a third rung (one-slot nodes), so the room left
 * is smaller than a mirror wherever it is; stderr prints
 * {@code stage <i> fill-begin} before each fill, and with a second argument
 * {@code diag} also {@code free-at-call=} ({@code Runtime.freeMemory()} read
 * between the drop and the call; {@code -1} without {@code diag}) after it.
 * stdout is unchanged.
 * stdout (HotSpot 25, any collector):
 * <pre>
 *   mode user
 *   stage 0 fill=oome getClass=ok name=GciOo22LazyMirrorOomeProbe$Lazy0
 *   stage 1 fill=oome getClass=ok name=GciOo22LazyMirrorOomeProbe$Lazy1
 *   stage 2 fill=oome getClass=ok name=GciOo22LazyMirrorOomeProbe$Lazy2
 *   PROBE-END
 * </pre>
 * ({@code vmexc}: the names are {@code java.lang.ArithmeticException},
 * {@code java.lang.NegativeArraySizeException},
 * {@code java.lang.ArrayStoreException}.) {@code getClass=oome} is the
 * pre-i22 CratonVM answer; a process abort prints nothing after the stage it
 * died in. Diagnostics on stderr.
 *
 * <p>{@code java -XX:+UseSerialGC -Xmx64m GciOo22LazyMirrorOomeProbe user}
 */
public final class GciOo22LazyMirrorOomeProbe {
    static final class Lazy0 {
        int a;
    }

    static final class Lazy1 {
        long b;
    }

    static final class Lazy2 {
        Object c;
    }

    static int zero = 0;
    static int negative = -1;

    /**
     * Fill the heap with live chains; they die when this returns. Three
     * rungs (gci i26/oo2): 128 KiB chunks, then small pairs, then one-slot
     * nodes (the smallest chain link), so what is left anywhere is smaller
     * than a {@code java.lang.Class} mirror. Answers the number of refusals
     * (3 when every rung ended in {@code OutOfMemoryError}).
     */
    static int fill() {
        Object[] head = null;
        int refusals = 0;
        try {
            while (true) {
                Object[] node = new Object[2];
                node[0] = head;
                node[1] = new long[16 * 1024];
                head = node;
            }
        } catch (OutOfMemoryError e) {
            refusals++;
        }
        try {
            while (true) {
                Object[] node = new Object[2];
                node[0] = head;
                node[1] = new long[2];
                head = node;
            }
        } catch (OutOfMemoryError e) {
            refusals++;
        }
        try {
            while (true) {
                Object[] node = new Object[1];
                node[0] = head;
                head = node;
            }
        } catch (OutOfMemoryError e) {
            refusals++;
        }
        // `head` still holds the heap here; returning drops it.
        return head == null ? -1 : refusals;
    }

    static Object[] makeUser() {
        return new Object[] {new Lazy0(), new Lazy1(), new Lazy2()};
    }

    static Object[] makeVmExceptions() {
        Object[] out = new Object[3];
        try {
            out[0] = Integer.valueOf(1 / zero);
        } catch (ArithmeticException e) {
            out[0] = e;
        }
        try {
            out[1] = new int[negative];
        } catch (NegativeArraySizeException e) {
            out[1] = e;
        }
        try {
            Object[] strings = new String[1];
            strings[0] = Integer.valueOf(7);
            out[2] = strings;
        } catch (ArrayStoreException e) {
            out[2] = e;
        }
        return out;
    }

    /**
     * gci i26/oo2: three bootstrap classes no boot path touches, instantiated
     * (so loaded and initialized) here. The VM loads them itself from the
     * runtime image, so no Java {@code defineClass} hands their mirror out
     * at load -- unlike a class-path class under {@code --jdk-only}, whose
     * mirror the app loader's {@code defineClass1} may already have made.
     */
    static Object[] makeBoot() {
        return new Object[] {
            new java.util.zip.Adler32(),
            new java.util.zip.CRC32C(),
            new java.util.concurrent.atomic.DoubleAdder()
        };
    }

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "user";
        Object[] objects = mode.equals("vmexc")
                ? makeVmExceptions()
                : (mode.equals("boot") ? makeBoot() : makeUser());
        // gci i26/oo2: a second argument `diag` reads Runtime.freeMemory()
        // between the drop and the call; the deciding rows leave it out, so
        // nothing but getClass runs there.
        boolean diag = args.length > 1 && args[1].equals("diag");
        Runtime rt = Runtime.getRuntime();
        if (diag) {
            // Link the native before any fill (a first call may resolve it).
            System.err.println("[gci-oo22] free-at-start=" + rt.freeMemory());
        }
        System.out.println("mode " + mode);
        for (int i = 0; i < objects.length; i++) {
            Object target = objects[i];
            System.err.println("[gci-oo22] stage " + i + " fill-begin");
            int filled = fill();
            // `diag` only: how much room the fill left at the call (a native
            // answering a long), for the stderr diagnostics.
            long freeAtCall = diag ? rt.freeMemory() : -1L;
            // The heap is full of garbage now; nothing allocates before getClass.
            Class<?> mirror = null;
            int code = 0;
            try {
                mirror = target.getClass();
            } catch (OutOfMemoryError e) {
                code = 1;
            } catch (Throwable t) {
                code = 2;
            }
            String fillWord = filled == 3 ? "oome" : (filled < 0 ? "none" : "partial");
            String codeWord = code == 0 ? "ok" : (code == 1 ? "oome" : "other");
            System.out.println("stage " + i + " fill=" + fillWord + " getClass=" + codeWord
                    + " name=" + (mirror == null ? "none" : mirror.getName()));
            System.err.println("[gci-oo22] stage " + i + " refusals=" + filled
                    + " free-at-call=" + freeAtCall);
        }
        System.out.println("PROBE-END");
    }
}
