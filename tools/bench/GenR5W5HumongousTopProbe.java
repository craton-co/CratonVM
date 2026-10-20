// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gen r5w5/old9 (2026-09-27): humongous arrays interleaved with promoted small
 * survivors leave the old generation FRAGMENTED when they die, unless they
 * were placed from the top of it — the probe
 * {@code docs/internal/gc/gengc-r5w1-oldgen5-proposal-humongous-arrays-from-the-top-of-old-gen-DONE-20260928.md}
 * asked for ({@code CRATONVM_GC_OLD_HUMONGOUS_TOP}).
 *
 * <p>Geometry at {@code -Xmx128m} (CratonVM): 32 MiB semi-spaces, a 64 MiB old
 * generation, humongous threshold 16 MiB. The program tenures what startup
 * left, then alternates a 17 MiB {@code byte[]} (humongous: straight to old
 * gen) with ~4 MiB of small survivors (promoted through the young generation),
 * twice, drops both arrays and asks for 33 MiB (more than a semi-space, so
 * the request cannot fall back to the young generation).
 *
 * <ul>
 *   <li>Best fit (default): {@code [startup][H1 17][S1 4][H2 17][S2 4][tail]}.
 *       The dead arrays leave two 17 MiB holes and a tail of about
 *       {@code 22 MiB - startup}; no hole holds 33 MiB although 50+ MiB are
 *       free. With the repair switched off ({@code CRATONVM_GC_OLD_OOM_COMPACT=0})
 *       that is a fragmentation {@code OutOfMemoryError}.</li>
 *   <li>From the top ({@code CRATONVM_GC_OLD_HUMONGOUS_TOP=1}):
 *       {@code [startup][S1][S2] ... [H2][H1]}. The dead arrays merge with the
 *       free space between S2 and them into one run of 50+ MiB.</li>
 * </ul>
 *
 * <p>HotSpot ({@code java -XX:+UseSerialGC -Xmx128m}) compacts on every full
 * collection and prints:
 * <pre>
 *   humongous-after-churn ok
 *   PASS
 * </pre>
 * Commands:
 * <pre>
 *   java -XX:+UseSerialGC -Xmx128m -cp tools/bench GenR5W5HumongousTopProbe
 *   CRATONVM_GC_OLD_HUMONGOUS_TOP=1 CRATONVM_GC_OLD_OOM_COMPACT=0 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx128m -cp tools/bench GenR5W5HumongousTopProbe
 *   CRATONVM_GC_OLD_OOM_COMPACT=0 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx128m -cp tools/bench GenR5W5HumongousTopProbe
 * </pre>
 * The second must print HotSpot's lines; the third (the negative control) prints
 * {@code humongous-after-churn FAILED: OutOfMemoryError "Java heap space"} and
 * {@code FAIL}. Run each with and without {@code --nojit}: a stale
 * conservative word holding a dead array keeps it (and fails both arms alike),
 * which the negative control then cannot tell apart.
 */
public final class GenR5W5HumongousTopProbe {
    static final int MIB = 1 << 20;
    static byte[] h1;
    static byte[] h2;
    static Object[] s1;
    static Object[] s2;

    /** Own frame: no local keeps the array after the static is cleared. */
    static void humongous(int which) {
        byte[] a = new byte[17 * MIB];
        a[0] = (byte) which;
        a[a.length - 1] = (byte) (which + 1);
        if (which == 1) {
            h1 = a;
        } else {
            h2 = a;
        }
    }

    /** ~4 MiB of small survivors, promoted by the collections that follow. */
    static Object[] survivors(int salt) {
        Object[] s = new Object[4096];
        for (int i = 0; i < s.length; i++) {
            byte[] b = new byte[1000];
            b[0] = (byte) (i + salt);
            s[i] = b;
        }
        return s;
    }

    static void tenure() {
        for (int i = 0; i < 4; i++) {
            System.gc();
        }
    }

    static boolean survivorsIntact(Object[] s, int salt) {
        for (int i = 0; i < s.length; i++) {
            if (((byte[]) s[i])[0] != (byte) (i + salt)) {
                return false;
            }
        }
        return true;
    }

    /** The request, in its own frame. */
    static byte[] request() {
        byte[] d = new byte[33 * MIB];
        d[d.length - 1] = 9;
        return d;
    }

    public static void main(String[] args) {
        tenure();
        humongous(1);
        s1 = survivors(1);
        tenure();
        humongous(2);
        s2 = survivors(2);
        tenure();
        boolean arraysOk = h1[h1.length - 1] == 2 && h2[h2.length - 1] == 3;
        h1 = null;
        h2 = null;
        System.gc();
        System.gc();
        boolean ok;
        try {
            byte[] d = request();
            ok = arraysOk && d[d.length - 1] == 9 && survivorsIntact(s1, 1) && survivorsIntact(s2, 2);
            System.out.println("humongous-after-churn " + (ok ? "ok" : "FAILED: data corrupted"));
        } catch (OutOfMemoryError e) {
            ok = false;
            System.out.println("humongous-after-churn FAILED: OutOfMemoryError \"" + e.getMessage() + "\"");
        }
        System.out.println(ok ? "PASS" : "FAIL");
        if (!ok) {
            System.exit(1);
        }
    }
}
