// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.reflect.Field;

/**
 * gci i22/oo ({@code gcg-g4oo-oome-ladder-review-findings} item 5): reflection
 * links the class it lists ({@code getDeclaredFields0} -> {@code link_class}),
 * and CratonVM builds a class's {@code ConstantValue} Strings at that link
 * (preparation), with an allocation that never collects. HotSpot builds them
 * with the mirror at class load. So on a heap full of GARBAGE, the first
 * {@code getDeclaredFields()} of a loaded-but-unlinked class threw
 * {@code OutOfMemoryError: Java heap space (prepare_class ... ConstantValue
 * String)} on CratonVM where HotSpot answers. After i22/oo the reflective
 * link collects once and links again.
 *
 * <p>Each stage loads (does not link or initialize) one nested class whose
 * eight {@code static final String} constants are about 60 000 characters
 * each (javac folds the concatenations into one {@code ConstantValue} each),
 * fills the heap with LIVE chunks (gci i26/oo2: 128 KiB, then 8 KiB, then
 * 2 KiB, each rung until its first error; i22 had the first rung only),
 * drops them by returning (garbage, not collected yet), and lists the
 * class's fields. The listing's own small Java-side allocations fit in what
 * the last rung left; the eight Strings (about 480 KB) do not. stderr:
 * {@code stage <i> fill-begin} before each fill; with the argument
 * {@code diag}, {@code free-at-call=} ({@code Runtime.freeMemory()} between
 * the drop and the listing, else {@code -1}). stdout is unchanged.
 *
 * <p>stdout (HotSpot 25, any collector, {@code -Xmx64m}):
 * <pre>
 *   stage 0 fill=oome getDeclaredFields=ok fields=8 sum=480032
 *   stage 1 fill=oome getDeclaredFields=ok fields=8 sum=480032
 *   stage 2 fill=oome getDeclaredFields=ok fields=8 sum=480032
 *   PROBE-END
 * </pre>
 * {@code getDeclaredFields=oome fields=-1 sum=-1} is the pre-i22 CratonVM
 * answer. {@code sum} reads every constant back (it initializes the class),
 * so a wrong String after the retry shows. Diagnostics on stderr.
 *
 * <p>{@code java -XX:+UseSerialGC -Xmx64m GciOo22ReflectLinkOomeProbe}
 */
public final class GciOo22ReflectLinkOomeProbe {
    static final String C10 = "0123456789";
    static final String C100 = C10 + C10 + C10 + C10 + C10 + C10 + C10 + C10 + C10 + C10;
    static final String C1K =
            C100 + C100 + C100 + C100 + C100 + C100 + C100 + C100 + C100 + C100;
    static final String C10K = C1K + C1K + C1K + C1K + C1K + C1K + C1K + C1K + C1K + C1K;
    static final String C60K = C10K + C10K + C10K + C10K + C10K + C10K;

    static final class Consts0 {
        static final String F0 = C60K + "c0f0";
        static final String F1 = C60K + "c0f1";
        static final String F2 = C60K + "c0f2";
        static final String F3 = C60K + "c0f3";
        static final String F4 = C60K + "c0f4";
        static final String F5 = C60K + "c0f5";
        static final String F6 = C60K + "c0f6";
        static final String F7 = C60K + "c0f7";
    }

    static final class Consts1 {
        static final String F0 = C60K + "c1f0";
        static final String F1 = C60K + "c1f1";
        static final String F2 = C60K + "c1f2";
        static final String F3 = C60K + "c1f3";
        static final String F4 = C60K + "c1f4";
        static final String F5 = C60K + "c1f5";
        static final String F6 = C60K + "c1f6";
        static final String F7 = C60K + "c1f7";
    }

    static final class Consts2 {
        static final String F0 = C60K + "c2f0";
        static final String F1 = C60K + "c2f1";
        static final String F2 = C60K + "c2f2";
        static final String F3 = C60K + "c2f3";
        static final String F4 = C60K + "c2f4";
        static final String F5 = C60K + "c2f5";
        static final String F6 = C60K + "c2f6";
        static final String F7 = C60K + "c2f7";
    }

    /**
     * Fill the heap with live chunks; they die when this returns. gci i26/oo2:
     * a ladder of rungs (128 KiB, then 8 KiB, then 2 KiB chunks, each until
     * its first error), so no space keeps room for even one 60 000-char
     * constant (the i22 single rung left whatever room smaller than 128 KiB
     * there was in each space, and the vi22-oo R2 kill twins never refused),
     * while the listing's few small Java-side objects (its
     * {@code ReflectionData} and {@code SoftReference}) still fit in what the
     * last rung left, so nothing collects before the native's link. Answers
     * the chunks held (all rungs).
     */
    static int fill() {
        Object[] head = null;
        int chunks = 0;
        int[] rungs = {16 * 1024, 1024, 256};
        for (int r = 0; r < rungs.length; r++) {
            int words = rungs[r];
            try {
                while (true) {
                    Object[] node = new Object[2];
                    node[0] = head;
                    node[1] = new long[words];
                    head = node;
                    chunks++;
                }
            } catch (OutOfMemoryError e) {
                // This rung is full; `head` holds every chunk until return.
            }
        }
        return head == null ? -1 : chunks;
    }

    public static void main(String[] args) throws Exception {
        ClassLoader loader = GciOo22ReflectLinkOomeProbe.class.getClassLoader();
        // gci i26/oo2: `diag` (first argument) reads Runtime.freeMemory()
        // between the drop and the listing; the deciding rows leave it out.
        boolean diag = args.length > 0 && args[0].equals("diag");
        Runtime rt = Runtime.getRuntime();
        if (diag) {
            // Link the native before any fill (a first call may resolve it).
            System.err.println("[gci-oo22] free-at-start=" + rt.freeMemory());
        }
        for (int i = 0; i < 3; i++) {
            // Loaded, neither linked nor initialized.
            Class<?> c = Class.forName("GciOo22ReflectLinkOomeProbe$Consts" + i, false, loader);
            System.err.println("[gci-oo22] stage " + i + " fill-begin");
            int chunks = fill();
            long freeAtCall = diag ? rt.freeMemory() : -1L;
            // The heap is full of garbage now; the listing is the next thing to run.
            Field[] fields = null;
            int code = 0;
            try {
                fields = c.getDeclaredFields();
            } catch (OutOfMemoryError e) {
                code = 1;
            } catch (Throwable t) {
                code = 2;
            }
            long sum = -1;
            if (fields != null) {
                sum = 0;
                for (Field f : fields) {
                    sum += ((String) f.get(null)).length();
                }
            }
            String codeWord = code == 0 ? "ok" : (code == 1 ? "oome" : "other");
            System.out.println("stage " + i + " fill=" + (chunks > 0 ? "oome" : "none")
                    + " getDeclaredFields=" + codeWord
                    + " fields=" + (fields == null ? -1 : fields.length)
                    + " sum=" + sum);
            System.err.println("[gci-oo22] stage " + i + " chunks=" + chunks
                    + " free-at-call=" + freeAtCall);
        }
        System.out.println("PROBE-END");
    }
}
