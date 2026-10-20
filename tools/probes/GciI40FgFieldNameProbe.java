// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.reflect.Field;

/**
 * gci i40/fg ({@code gcg-g4oo-oome-ladder-review-findings} item 5, Batch 9b
 * R3): which read of a listed static field breaks after the OOME fill.
 *
 * <p>{@code GciRf38WideReflectLinkOomeProbe} under {@code --compatible}
 * Generational threw {@code IllegalArgumentException: Unsafe access with a
 * null base ... : 0x10} at stage 1's {@code Field.get(null)}: the JDK's field
 * accessor reached a {@code DirectMethodHandle$StaticAccessor} whose
 * {@code staticBase} is null, which {@code MethodHandleNatives.staticFieldBase}
 * answers only when it cannot read the member's {@code clazz} mirror or its
 * {@code name} String. This probe runs the same two stages over the same
 * nested classes (it needs {@code GciRf38WideReflectLinkOomeProbe$Wide0} and
 * {@code $Wide1} on the class path; it compiles nothing large) and, before
 * the reads, checks every listed field's NAME: {@code names} counts the
 * distinct names of the form {@code F<0..255>}. A pooled name String handed
 * out after its collection reads back wrong here in either mode, also where
 * {@code Field.get} never reads the name ({@code --jdk-only}'s field doors).
 *
 * <p>stdout (HotSpot 25, any collector, {@code -Xmx64m}):
 * <pre>
 *   stage 0 fill=oome getDeclaredFields=ok fields=256 names=256 get=ok sum=15361024
 *   stage 1 fill=oome getDeclaredFields=ok fields=256 names=256 get=ok sum=15361024
 *   PROBE-END
 * </pre>
 * stderr ({@code [gci-i40fg]}): {@code fill-begin}, the identity-interned
 * count of the names (not compared), the read's throwable if one was thrown,
 * and {@code chunks=}.
 *
 * <p>{@code java -XX:+UseSerialGC -Xmx64m GciI40FgFieldNameProbe}
 */
public final class GciI40FgFieldNameProbe {

    /** Fill the heap with live chunks; they die when this returns. Answers the chunks held. */
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

    /** The index of a name {@code F<0..255>}, else -1. */
    static int fieldIndex(String n) {
        if (n == null || n.length() < 2 || n.length() > 4 || n.charAt(0) != 'F') {
            return -1;
        }
        int v = 0;
        for (int k = 1; k < n.length(); k++) {
            char ch = n.charAt(k);
            if (ch < '0' || ch > '9') {
                return -1;
            }
            v = v * 10 + (ch - '0');
        }
        return v < 256 ? v : -1;
    }

    public static void main(String[] args) throws Exception {
        ClassLoader loader = GciI40FgFieldNameProbe.class.getClassLoader();
        for (int i = 0; i < 2; i++) {
            // Loaded, neither linked nor initialized.
            Class<?> c = Class.forName("GciRf38WideReflectLinkOomeProbe$Wide" + i, false, loader);
            System.err.println("[gci-i40fg] stage " + i + " fill-begin");
            int chunks = fill();
            // The heap is full of garbage now; the listing is the next thing to run.
            Field[] fields = null;
            String listing = "ok";
            try {
                fields = c.getDeclaredFields();
            } catch (OutOfMemoryError e) {
                listing = "oome";
            } catch (Throwable t) {
                listing = "other";
            }
            int names = 0;
            int interned = 0;
            String get = "none";
            long sum = -1;
            if (fields != null) {
                boolean[] seen = new boolean[256];
                for (Field f : fields) {
                    String n = f.getName();
                    int idx = fieldIndex(n);
                    if (idx >= 0 && !seen[idx]) {
                        seen[idx] = true;
                        names++;
                    }
                    if (n != null && n == n.intern()) {
                        interned++;
                    }
                }
                get = "ok";
                sum = 0;
                try {
                    for (Field f : fields) {
                        sum += ((String) f.get(null)).length();
                    }
                } catch (Throwable t) {
                    get = t.getClass().getName();
                    sum = -1;
                    System.err.println("[gci-i40fg] stage " + i + " get-threw " + t);
                }
            }
            System.out.println("stage " + i + " fill=" + (chunks > 0 ? "oome" : "none")
                    + " getDeclaredFields=" + listing
                    + " fields=" + (fields == null ? -1 : fields.length)
                    + " names=" + names
                    + " get=" + get
                    + " sum=" + sum);
            System.err.println("[gci-i40fg] stage " + i + " interned=" + interned
                    + " chunks=" + chunks);
        }
        System.out.println("PROBE-END");
    }
}
