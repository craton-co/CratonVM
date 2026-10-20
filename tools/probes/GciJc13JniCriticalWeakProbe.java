/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2026 Craton Software Company
 */

import java.nio.ByteBuffer;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

/**
 * gci i13/jc (2026-10-13): JNI critical regions, weak globals, local frames
 * and direct buffers against HotSpot 25. Every stdout line is HotSpot's exact
 * answer; diagnostics go to stderr. Native half:
 * {@code tools/probes/jni/GciJc13JniCriticalWeakProbe.c}.
 *
 * <p>Usage: {@code GciJc13JniCriticalWeakProbe /abs/path/gcijc13.dll [mode]},
 * mode {@code all} (default: the deterministic sections) or {@code poll} (a
 * native polls {@code IsSameObject(w, NULL)} while the heap churns: run it on
 * G1, whose concurrent cycles are the ones a keep-alive in the poll could
 * defeat).
 *
 * <ul>
 *   <li>{@code critical-abort}: GetPrimitiveArrayCritical, the native writes,
 *       ReleasePrimitiveArrayCritical(JNI_ABORT); HotSpot pins (isCopy 0) and
 *       ignores the mode, so the writes are in the array;
 *   <li>{@code critical-commit}: the same with mode 0 (both VMs keep writes);
 *   <li>{@code chars} / {@code critical-chars} / {@code region}: the exact
 *       UTF-16 units of a string with a lone surrogate, through GetStringChars,
 *       GetStringCritical and GetStringRegion, and whether GetStringChars'
 *       buffer is NUL-terminated (HotSpot allocates len + 1);
 *   <li>{@code dbb-*}: NewDirectByteBuffer's byte order and typed reads, a NULL
 *       address, and capacities outside {@code [0, Integer.MAX_VALUE]};
 *   <li>{@code reftype}: GetObjectRefType of local / global / weak / deleted
 *       handles and of PopLocalFrame's answer for a global result;
 *   <li>{@code capacity}: EnsureLocalCapacity / PushLocalFrame return codes
 *       and whether an exception is pending, for -1, 0, 65536, 65537, MAX;
 *   <li>{@code weak}: a weak global is not cleared while its referent is
 *       strongly reachable and reads NULL after the referent's last strong
 *       path is gone and System.gc() ran.
 * </ul>
 */
public final class GciJc13JniCriticalWeakProbe {
    /** Critical-section write test: returns isCopy; mode 2 = JNI_ABORT. */
    static native int criticalScale(int[] a, int mode);

    static native String charsRoundTrip(String s);

    static native int charsTerminated(String s);

    static native String criticalRoundTrip(String s);

    static native String regionRoundTrip(String s);

    /** NewDirectByteBuffer over a 64-byte native buffer whose first 4 bytes are 00 00 00 01. */
    static native ByteBuffer newDirect(long cap);

    static native ByteBuffer newDirectNull();

    static native int directAddressIsNull(ByteBuffer b);

    /** GetDirectBufferAddress == NULL ? 1 : 0, then GetDirectBufferCapacity, of any Buffer. */
    static native long directViewProbe(java.nio.Buffer b);

    /** GetStringCritical's isCopy for s (0 / 1). */
    static native int stringCriticalIsCopy(String s);

    /** out = {local, global, weak, deleted global, deleted weak, pop(global), same-after-delete}. */
    static native void refTypes(Object o, int[] out);

    /** out = {rc, exception pending (0/1)}; ensure = EnsureLocalCapacity, else PushLocalFrame (+ pop). */
    static native void capacity(int cap, boolean ensure, int[] out);

    static native void makeWeak(Object o);

    static native boolean weakIsSame(Object o);

    static native boolean weakCleared();

    static native void deleteWeak();

    static String units(String s) {
        StringBuilder sb = new StringBuilder("[");
        for (int i = 0; i < s.length(); i++) {
            if (i > 0) {
                sb.append(' ');
            }
            sb.append(Integer.toHexString(s.charAt(i)));
        }
        return sb.append(']').toString();
    }

    static void critical() {
        int[] a = {1, 2, 3};
        int isCopy = criticalScale(a, 2);
        System.out.println("critical-abort: " + Arrays.toString(a) + " isCopy=" + isCopy);
        int[] b = {1, 2, 3};
        criticalScale(b, 0);
        System.out.println("critical-commit: " + Arrays.toString(b));
        // i13/jc second pass (F11): a large array through the bulk copy both ways.
        int[] big = new int[200000];
        for (int i = 0; i < big.length; i++) {
            big[i] = i - 100000;
        }
        criticalScale(big, 0);
        long sum = 0;
        for (int v : big) {
            sum += v;
        }
        System.out.println("critical-big: sum=" + sum + " first=" + big[0] + " last="
                + big[big.length - 1]);
    }

    static void strings() {
        String s = "a\uD800b";
        System.out.println("chars: " + units(charsRoundTrip(s)));
        System.out.println("chars-terminated: " + charsTerminated(s) + " empty: "
                + charsTerminated(""));
        System.out.println("critical-chars: " + units(criticalRoundTrip(s)));
        System.out.println("region: " + units(regionRoundTrip(s)));
        System.out.println("chars-empty: " + units(charsRoundTrip("")));
        // i13/jc second pass (F14): HotSpot copies a LATIN1 string (1) and pins
        // a UTF16 one (0).
        System.out.println("critical-chars-iscopy: latin1=" + stringCriticalIsCopy("abc")
                + " utf16=" + stringCriticalIsCopy("aαb"));
    }

    static void directBuffers() {
        ByteBuffer b = newDirect(64);
        System.out.println("dbb-order: " + b.order() + " getInt=" + b.getInt(0) + " direct="
                + b.isDirect() + " cap=" + b.capacity() + " limit=" + b.limit() + " pos="
                + b.position());
        // i13/jc second pass (F16): an int view of a JDK-allocated direct
        // buffer is a DirectBuffer; its capacity is in ints.
        long view = directViewProbe(ByteBuffer.allocateDirect(64).asIntBuffer());
        System.out.println("dbb-int-view: addressNull=" + (view >> 32) + " cap=" + (int) view);
        ByteBuffer n = newDirectNull();
        System.out.println("dbb-null: " + (n == null ? "null" : ("cap=" + n.capacity()
                + " addressNull=" + directAddressIsNull(n))));
        for (long cap : new long[] {-1L, 1L << 31, 1L << 32}) {
            try {
                ByteBuffer x = newDirect(cap);
                System.out.println("dbb-cap " + cap + ": "
                        + (x == null ? "null" : ("cap=" + x.capacity())));
            } catch (IllegalArgumentException e) {
                System.out.println("dbb-cap " + cap + ": IAE " + e.getMessage());
            }
        }
    }

    static void printRefTypes() {
        int[] out = new int[7];
        refTypes(new Object(), out);
        System.out.println("reftype: local=" + out[0] + " global=" + out[1] + " weak=" + out[2]
                + " deleted-global=" + out[3] + " deleted-weak=" + out[4] + " pop-global="
                + out[5] + " same-after-delete=" + out[6]);
    }

    static void capacities() {
        int[] out = new int[2];
        for (boolean ensure : new boolean[] {true, false}) {
            StringBuilder sb = new StringBuilder(ensure ? "capacity-ensure:" : "capacity-push:");
            for (int cap : new int[] {-1, 0, 65536, 65537, Integer.MAX_VALUE}) {
                capacity(cap, ensure, out);
                sb.append(' ').append(cap).append("->").append(out[0]).append(out[1] == 1 ? "+exc" : "");
            }
            System.out.println(sb);
        }
    }

    static void weak() {
        Object o = new int[16];
        makeWeak(o);
        System.out.println("weak-live: same=" + weakIsSame(o) + " cleared=" + weakCleared());
        o = null;
        System.gc();
        System.out.println("weak-after-gc: cleared=" + weakCleared());
        deleteWeak();
    }

    static volatile Object sink;

    /**
     * The referent is held strongly across many young collections (so it is
     * old), dropped, then the heap churns: a 24 MB ring of 256 KB arrays (below
     * G1's humongous threshold at 1 MB regions) replaced one slot per round
     * keeps old occupancy above the initiating threshold, so concurrent cycles
     * keep starting, and the native polls {@code IsSameObject(w, NULL)} after
     * every round. HotSpot clears the weak at the first remark after the drop.
     */
    static void poll() {
        Object o = new int[16];
        makeWeak(o);
        List<byte[]> ring = new ArrayList<>();
        for (int i = 0; i < 96; i++) {
            ring.add(new byte[256 * 1024]);
        }
        // ~250 MB of young garbage while `o` is held: more young collections
        // than the default tenuring threshold, so `o` is old when dropped (a
        // young referent is cleared by the next young pause, which tests
        // nothing about the concurrent cycle).
        for (int i = 0; i < 4000; i++) {
            sink = new byte[64 * 1024];
        }
        System.out.println("poll-live: same=" + weakIsSame(o));
        o = null;
        boolean cleared = false;
        int round = 0;
        final int rounds = 20000;
        for (; round < rounds && !cleared; round++) {
            ring.set(round % ring.size(), new byte[256 * 1024]);
            for (int k = 0; k < 8; k++) {
                sink = new byte[16 * 1024];
            }
            cleared = weakCleared();
        }
        System.err.println("[gci-jc13-poll] cleared=" + cleared + " rounds=" + round);
        System.out.println("poll-cleared=" + cleared);
        deleteWeak();
    }

    public static void main(String[] args) {
        if (args.length < 1) {
            System.out.println("usage: GciJc13JniCriticalWeakProbe /abs/path/gcijc13.dll [all|poll]");
            return;
        }
        System.load(args[0]);
        String mode = args.length > 1 ? args[1] : "all";
        if (mode.equals("poll")) {
            poll();
        } else {
            critical();
            strings();
            directBuffers();
            printRefTypes();
            capacities();
            weak();
        }
        System.out.println("done");
    }
}
