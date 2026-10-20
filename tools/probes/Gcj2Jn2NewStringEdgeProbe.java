// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gcj j2/jn2 (2026-10-19): the two edges of JNI {@code NewString}
 * ({@code docs/known-issues/gc/gcj-j2jn2-jni-new-string-edges-are-not-hotspots-20261019.md}).
 * Native half: {@code tools/probes/jni/Gcj2Jn2NewStringEdgeProbe.c} (build line
 * there).
 *
 * <p>HotSpot ({@code java -XX:+UseSerialGC}) prints:
 *
 * <pre>
 *   null-zero: "" length=0
 *   buf-zero: "" length=0
 *   negative: java.lang.NegativeArraySizeException: -1
 *   control: "ab" length=2
 * </pre>
 *
 * <p>Usage: {@code Gcj2Jn2NewStringEdgeProbe <library>}.
 */
public final class Gcj2Jn2NewStringEdgeProbe {

    /** {@code NewString(mode == 0 ? NULL : buf, len)}; {@code buf} holds "ab". */
    static native String newString(int mode, int len);

    static void run(String name, int mode, int len) {
        try {
            String s = newString(mode, len);
            if (s == null) {
                System.out.println(name + ": null");
            } else {
                System.out.println(name + ": \"" + s + "\" length=" + s.length());
            }
        } catch (Throwable t) {
            System.out.println(name + ": " + t);
        }
    }

    public static void main(String[] args) {
        System.load(args[0]);
        run("null-zero", 0, 0);
        run("buf-zero", 1, 0);
        run("negative", 1, -1);
        run("control", 1, 2);
    }
}
