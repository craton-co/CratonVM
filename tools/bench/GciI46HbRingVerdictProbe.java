// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.ArrayList;
import java.util.List;

/**
 * gci i46/hb ({@code docs/known-issues/gc/gci-i46hb-reclaim-guard-ring-verdicts-cry-wolf-on-a-legitimate-cast-20261016.md}):
 * a LEGITIMATE failing {@code checkcast} of a live object that the allocator
 * placed in memory an earlier collection freed. HotSpot prints nothing on
 * stderr; CratonVM's reclaim guard printed its reclamation-ring lines at
 * {@code ERROR} (the launcher's default stderr floor).
 *
 * <pre>
 *   java     -XX:+UseSerialGC        -Xmx256m -cp &lt;classes&gt; GciI46HbRingVerdictProbe [rounds]
 *   cratonvm -XX:+UseGenerationalGC  -Xmx256m -cp &lt;classes&gt; GciI46HbRingVerdictProbe [rounds]
 * </pre>
 *
 * Each round (default 6, at most 6) builds scattered garbage, collects twice
 * with {@code System.gc()} (the in-place sweeps that feed the young and old
 * freed rings), allocates fresh objects of ONE receiver class per round (so
 * the reused spans hold live objects), and casts 16 of them to 16 unrelated
 * types: 6 x 16 distinct (receiver class, target) pairs, each a first
 * sighting for the forensics gate. Every cast must fail. stdout is the same
 * on both VMs: {@code casts=N caught=N} and {@code PASS-SHAPE}.
 */
public final class GciI46HbRingVerdictProbe {
    static final class H0 { long a; Object b; H0(long a) { this.a = a; } }
    static final class H1 { long a; Object b; H1(long a) { this.a = a; } }
    static final class H2 { long a; Object b; H2(long a) { this.a = a; } }
    static final class H3 { long a; Object b; H3(long a) { this.a = a; } }
    static final class H4 { long a; Object b; H4(long a) { this.a = a; } }
    static final class H5 { long a; Object b; H5(long a) { this.a = a; } }

    static volatile Object sink;

    static Object make(int kind, long i) {
        switch (kind) {
            case 0: return new H0(i);
            case 1: return new H1(i);
            case 2: return new H2(i);
            case 3: return new H3(i);
            case 4: return new H4(i);
            default: return new H5(i);
        }
    }

    static Object castTo(int t, Object o) {
        switch (t) {
            case 0: return (Integer) o;
            case 1: return (Long) o;
            case 2: return (Short) o;
            case 3: return (Byte) o;
            case 4: return (Double) o;
            case 5: return (Float) o;
            case 6: return (Character) o;
            case 7: return (Boolean) o;
            case 8: return (String) o;
            case 9: return (java.util.List<?>) o;
            case 10: return (java.util.Map<?, ?>) o;
            case 11: return (Runnable) o;
            case 12: return (CharSequence) o;
            case 13: return (Number) o;
            case 14: return (java.util.Set<?>) o;
            default: return (Thread) o;
        }
    }

    public static void main(String[] args) {
        int rounds = args.length > 0 ? Math.min(6, Integer.parseInt(args[0])) : 6;
        int casts = 0;
        int caught = 0;
        for (int r = 0; r < rounds; r++) {
            List<Object> keep = new ArrayList<>();
            for (int i = 0; i < 200_000; i++) {
                Object o = make(r, i);
                if ((i & 63) == 0) {
                    keep.add(o);
                }
            }
            sink = keep;
            System.gc();
            keep = null;
            sink = null;
            System.gc();
            Object[] fresh = new Object[4096];
            for (int i = 0; i < fresh.length; i++) {
                fresh[i] = make(r, i);
            }
            for (int t = 0; t < 16; t++) {
                Object o = fresh[(t * 257 + r * 31) % fresh.length];
                casts++;
                try {
                    sink = castTo(t, o);
                } catch (ClassCastException e) {
                    caught++;
                }
            }
            sink = fresh;
        }
        System.out.println("casts=" + casts + " caught=" + caught);
        System.out.println(caught == casts ? "PASS-SHAPE" : "FAIL-SHAPE");
    }
}
