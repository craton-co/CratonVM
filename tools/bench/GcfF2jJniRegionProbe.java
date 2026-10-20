// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gcf f2/j (2026-10-01): the edges of the JNI region functions, one
 * deterministic stdout line each; HotSpot 25 {@code -XX:+UseSerialGC} is the
 * oracle (this probe DECIDES what HotSpot does; the expected lines are not
 * assumed):
 * <ul>
 *   <li>{@code utf-tail-N}: the byte {@code GetStringUTFRegion} leaves right
 *       after an {@code N}-unit region ({@code 0} = it wrote a terminator,
 *       {@code 88} = the buffer's 'X' fill);</li>
 *   <li>{@code int-region ...} / {@code string-region ...}: the exception
 *       class and message an out-of-range or negative window raises
 *       ({@code none} when nothing is thrown);</li>
 *   <li>{@code field-id ...}: {@code GetFieldID} / {@code GetStaticFieldID}
 *       of a field of the other static-ness, or of none: {@code found=...}
 *       or the pending exception.</li>
 * </ul>
 * Compare this VM's output with HotSpot's line by line; see
 * {@code docs/internal/gc/gcf-f2j-jni-region-edges-diverge-from-hotspot-20261001.md}.
 *
 * <p>Commands (Linux):
 * <pre>
 *   gcc -O1 -shared -fPIC -I"$JDK/include" -I"$JDK/include/linux" \
 *       -o /tmp/libgcff2jjniregion.so tools/probes/jni/GcfF2jJniRegionProbe.c
 *   javac -d /tmp/gcff2jjniregion tools/bench/GcfF2jJniRegionProbe.java
 *   java -XX:+UseSerialGC -cp /tmp/gcff2jjniregion GcfF2jJniRegionProbe /tmp/libgcff2jjniregion.so
 * </pre>
 */
public final class GcfF2jJniRegionProbe {

    static native int utfRegionTail(String s, int start, int len);

    static native void intRegion(int[] a, int start, int len);

    static native void stringRegion(String s, int start, int len);

    static native boolean fieldId(Class<?> c, String name, boolean isStatic);

    static int staticInt = 1;
    int instanceInt = 2;

    interface Call {
        void run();
    }

    interface Ask {
        boolean run();
    }

    static void found(String name, Ask c) {
        String v;
        try {
            v = "found=" + c.run();
        } catch (Throwable t) {
            v = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + ": " + v);
    }

    static void raise(String name, Call c) {
        String v;
        try {
            c.run();
            v = "none";
        } catch (Throwable t) {
            v = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + ": " + v);
    }

    public static void main(String[] args) {
        System.load(args[0]);
        String s = "abcdefgh";
        System.out.println("utf-tail-3: " + utfRegionTail(s, 2, 3));
        System.out.println("utf-tail-0: " + utfRegionTail(s, 2, 0));
        int[] a = new int[] {1, 2, 3, 4};
        raise("int-region past-end", () -> intRegion(a, 2, 3));
        raise("int-region neg-start", () -> intRegion(a, -1, 2));
        raise("int-region neg-len", () -> intRegion(a, 0, -1));
        raise("int-region empty-at-end", () -> intRegion(a, 4, 0));
        raise("string-region past-end", () -> stringRegion(s, 6, 3));
        raise("string-region neg-start", () -> stringRegion(s, -1, 2));
        raise("string-region neg-len", () -> stringRegion(s, 0, -1));
        Class<?> me = GcfF2jJniRegionProbe.class;
        found("field-id instance", () -> fieldId(me, "instanceInt", false));
        found("field-id static", () -> fieldId(me, "staticInt", true));
        found("field-id instance-as-static", () -> fieldId(me, "instanceInt", true));
        found("field-id static-as-instance", () -> fieldId(me, "staticInt", false));
        found("field-id missing", () -> fieldId(me, "nope", false));
    }
}
