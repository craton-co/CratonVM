// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// The GPU input-residency cache is keyed by `ObjectRef` -- a raw heap
// address. A moving collector invalidates that key: after a relocation
// the Java array lives somewhere else, and `input_cache::remap_and_sweep`
// has to rewrite every entry's key to the collector's new address and
// drop the entries whose arrays died.
//
// On 2026-09-02 `short[]` and `byte[]` became offloadable, which means
// `CachedBuffer::I16`/`I8` entries now appear in that cache for the
// first time. The remap is generic over entries, so it *should* carry
// them -- but "should" is exactly the word that preceded the gap those
// two types were already in, so it is measured here instead.
//
// The failure this is built to catch is a stale or mis-keyed entry: a
// submit after a relocation that reads a device buffer belonging to the
// array's OLD address, or to a different array that has since been
// allocated there (an ABA on the address). Either produces wrong values,
// not a crash.
//
// Shape of each round:
//
//   1. submit a kernel so the input array becomes device-resident
//   2. allocate hard, so a collection runs and can relocate it
//   3. submit again and check the result against the value computed
//      from the array's CURRENT contents
//
// Garbage is allocated in shapes that survive to the old generation as
// well as shapes that die immediately, so both a young collection and a
// full one can happen between submits. The retained list is deliberately
// churned rather than only grown, so the collector has something to
// actually move rather than just space to fill.
//
// Every scenario prints one checksum. The runner compares `--gpu`
// against `--nojit` on the same binary and against HotSpot.
//
// Usage: java GpuResidencyGc [scenario] [n] [rounds]
//   0 all   1 short   2 byte   3 int   4 mixed-types-interleaved
//   5 fresh-array-per-round (the only one that relocates a cached array)
public class GpuResidencyGc {

    static void scaleS(short[] in, short[] out) {
        for (int i = 0; i < out.length; i++) out[i] = (short) (in[i] * 3 - 7);
    }

    static void scaleB(byte[] in, byte[] out) {
        for (int i = 0; i < out.length; i++) out[i] = (byte) (in[i] * 3 - 7);
    }

    static void scaleI(int[] in, int[] out) {
        for (int i = 0; i < out.length; i++) out[i] = in[i] * 3 - 7;
    }

    static long mix(long h, long v) {
        return h * 1000003L + v;
    }

    // Kept alive across rounds so the collector has live data to move,
    // and churned so it is not simply a monotonically growing region.
    static java.util.ArrayList<Object> retained = new java.util.ArrayList<>();

    /// Allocate enough to provoke a collection, with a mix of lifetimes.
    static void churn(int round) {
        // Short-lived: dies before the next collection.
        long acc = 0;
        for (int i = 0; i < 400; i++) {
            byte[] junk = new byte[8192];
            junk[i % junk.length] = (byte) i;
            acc += junk[i % junk.length];
        }
        // Medium-lived: retained for a few rounds, then dropped, so the
        // collector has to relocate survivors rather than only sweep.
        retained.add(new long[2048]);
        if (retained.size() > 24) {
            retained.subList(0, 12).clear();
        }
        if (acc == Long.MIN_VALUE) {
            System.out.println("unreachable");
        }
    }

    static long roundsShort(int n, int rounds) {
        short[] in = new short[n];
        short[] out = new short[n];
        for (int i = 0; i < n; i++) in[i] = (short) (i - 32768);
        long h = 0;
        for (int r = 0; r < rounds; r++) {
            scaleS(in, out);
            h = mix(h, checksumS(out));
            churn(r);
            // Mutate AFTER the churn, so the next submit must both see
            // the relocation and see the host write.
            in[r % n] = (short) (r * 7 - 15000);
            scaleS(in, out);
            h = mix(h, checksumS(out));
        }
        return h;
    }

    static long roundsByte(int n, int rounds) {
        byte[] in = new byte[n];
        byte[] out = new byte[n];
        for (int i = 0; i < n; i++) in[i] = (byte) (i - 128);
        long h = 0;
        for (int r = 0; r < rounds; r++) {
            scaleB(in, out);
            h = mix(h, checksumB(out));
            churn(r);
            in[r % n] = (byte) (r * 5 - 100);
            scaleB(in, out);
            h = mix(h, checksumB(out));
        }
        return h;
    }

    static long roundsInt(int n, int rounds) {
        int[] in = new int[n];
        int[] out = new int[n];
        for (int i = 0; i < n; i++) in[i] = i % 30011;
        long h = 0;
        for (int r = 0; r < rounds; r++) {
            scaleI(in, out);
            h = mix(h, checksumI(out));
            churn(r);
            in[r % n] = r * 31 - 5000;
            scaleI(in, out);
            h = mix(h, checksumI(out));
        }
        return h;
    }

    // All three types live in the cache at once, so a single remap has
    // to carry I32, I16 and I8 entries together. If the remap handled
    // only the widths that existed before 2026-09-02, this is where a
    // survivor of the wrong width shows up.
    static long roundsMixed(int n, int rounds) {
        short[] sIn = new short[n], sOut = new short[n];
        byte[] bIn = new byte[n], bOut = new byte[n];
        int[] iIn = new int[n], iOut = new int[n];
        for (int i = 0; i < n; i++) {
            sIn[i] = (short) (i - 32768);
            bIn[i] = (byte) (i - 128);
            iIn[i] = i % 30011;
        }
        long h = 0;
        for (int r = 0; r < rounds; r++) {
            scaleS(sIn, sOut);
            scaleB(bIn, bOut);
            scaleI(iIn, iOut);
            churn(r);
            sIn[r % n] = (short) (r * 3);
            bIn[r % n] = (byte) (r * 2);
            iIn[r % n] = r * 11;
            scaleS(sIn, sOut);
            scaleB(bIn, bOut);
            scaleI(iIn, iOut);
            h = mix(h, checksumS(sOut));
            h = mix(h, checksumB(bOut));
            h = mix(h, checksumI(iOut));
        }
        return h;
    }

    static long checksumS(short[] v) {
        long h = 0;
        for (short x : v) h = mix(h, x);
        return h;
    }

    static long checksumB(byte[] v) {
        long h = 0;
        for (byte x : v) h = mix(h, x);
        return h;
    }

    static long checksumI(int[] v) {
        long h = 0;
        for (int x : v) h = mix(h, x);
        return h;
    }

    // ── 5. a FRESH array each round: the only shape that relocates ───
    //
    // Scenarios 1-4 reuse one input array for every round, which turns
    // out to exercise nothing: the array is long-lived, tenures within
    // the first collection or two, and an old generation collected by
    // mark-sweep never moves it again. Instrumented under
    // `-Xmx64m -XX:+UseGenerationalGC` those scenarios drive 25
    // collections that move ~35000 objects and produce ZERO
    // `input_cache` remap events -- the cached arrays are simply never
    // among the objects that move.
    //
    // To put a cached array through a relocation it has to be YOUNG
    // while it is resident: allocate it, submit (which makes it
    // device-resident and enters it in the address-keyed cache), then
    // force a young collection, then submit again. Now the collection
    // has a live young array to copy, and `remap_and_sweep` has an
    // entry whose key actually moved.
    //
    // The previous round's array is dropped each iteration, so the
    // sweep half is exercised too: its cache entry must be removed
    // rather than left pointing at an address that is about to be
    // handed to a different allocation.
    static long roundsFresh(int n, int rounds) {
        long h = 0;
        for (int r = 0; r < rounds; r++) {
            short[] sIn = new short[n];
            short[] sOut = new short[n];
            byte[] bIn = new byte[n];
            byte[] bOut = new byte[n];
            for (int i = 0; i < n; i++) {
                sIn[i] = (short) (i + r);
                bIn[i] = (byte) (i + r);
            }
            // Resident now, and young.
            scaleS(sIn, sOut);
            scaleB(bIn, bOut);
            h = mix(h, checksumS(sOut));
            h = mix(h, checksumB(bOut));

            churn(r);

            // Same arrays, after a collection that may have moved them.
            // If the remap mis-keyed the entry this reads another
            // array's device buffer and the checksum diverges.
            scaleS(sIn, sOut);
            scaleB(bIn, bOut);
            h = mix(h, checksumS(sOut));
            h = mix(h, checksumB(bOut));

            // A host write after the relocation: the invalidation has to
            // find the entry under its NEW key, not the old one.
            sIn[r % n] = (short) (r * 13);
            bIn[r % n] = (byte) (r * 7);
            scaleS(sIn, sOut);
            scaleB(bIn, bOut);
            h = mix(h, checksumS(sOut));
            h = mix(h, checksumB(bOut));
        }
        return h;
    }

    public static void main(String[] args) {
        int which = args.length > 0 ? Integer.parseInt(args[0]) : 0;
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 131072;
        int rounds = args.length > 2 ? Integer.parseInt(args[2]) : 24;

        if (which == 0 || which == 1) System.out.println("gc_short=" + roundsShort(n, rounds));
        if (which == 0 || which == 2) System.out.println("gc_byte=" + roundsByte(n, rounds));
        if (which == 0 || which == 3) System.out.println("gc_int=" + roundsInt(n, rounds));
        if (which == 0 || which == 4) System.out.println("gc_mixed=" + roundsMixed(n, rounds));
        if (which == 0 || which == 5) System.out.println("gc_fresh=" + roundsFresh(n, rounds));
        System.out.println("GC_RESIDENCY_DONE n=" + n + " rounds=" + rounds);
    }
}
