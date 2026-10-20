// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.reflect.Array;
import java.util.ArrayList;

/**
 * zr z4/ab: VM-internal allocations made AFTER start-up, on a full ZGC heap.
 * Each mode fills the heap to the ceiling with 1 KiB chunks, frees
 * {@code slack} KiB of them (so the trigger's own Java allocations may still
 * fit and the refusal lands on a VM-internal object instead), then does ONE
 * first-time action whose VM side allocates:
 *
 * <ul>
 *   <li>{@code layout} -- first touch of {@code java.lang.foreign.ValueLayout}
 *       ({@code --compatible} runs the VM's layout preseed in
 *       {@code prepare_class}: objects, an {@code Optional}, a carrier mirror);
 *   <li>{@code bigint} -- first touch of {@code BigInteger} (the
 *       post-{@code <clinit>} constant fixup's {@code int[]} magnitudes);
 *   <li>{@code unsafe} -- first touch of {@code sun.misc.Unsafe} (the
 *       memory-access latch's class mirror);
 *   <li>{@code stdin} -- first read of {@code System.in} (the lazy carrier
 *       objects, where the VM makes them);
 *   <li>{@code forname} -- {@code Class.forName} of JDK classes nothing loaded
 *       yet (class definition + mirror);
 *   <li>{@code arrays} -- {@code Array.newInstance} one dimension deeper each
 *       time (a new array class and its mirror per step);
 *   <li>{@code all} -- every mode above in turn, each on a refilled heap.
 * </ul>
 *
 * <p>Usage: {@code Zr4AbNoAbort <mode> [slackKiB]}. stdout is deterministic
 * ({@code <mode>: survived}, then {@code done}); whether the action threw
 * {@code OutOfMemoryError} or fitted goes to stderr ({@code [zr4ab] ...}),
 * because the exact byte at which it fits differs between VMs. Pass: rc 0 and
 * {@code done}. The base binary aborts on the slack values where the refusal
 * lands on an infallible VM-internal allocation (rc 134, a
 * {@code FATAL: ZGC(real): ... cannot report failure} block on stderr), so
 * the host sweeps {@code slack} (0 1 4 16 64 256).
 */
public class Zr4AbNoAbort {
    static ArrayList<Object> fill;
    static boolean badResult;
    static Object layoutResult;

    public static void main(String[] args) throws Exception {
        // Resolve the diagnostic concat bootstrap before exhaustion. A cold
        // reporting call site can poison a JDK linker class under OOM and make
        // the recovery report itself fail (also reproducible on HotSpot Serial).
        formatOutcome("warmup", 0, "warmup");
        String mode = args.length > 0 ? args[0] : "all";
        int slack = args.length > 1 ? Integer.parseInt(args[1]) : 4;
        String[] modes = mode.equals("all")
                ? new String[] {"forname", "arrays", "unsafe", "bigint", "layout", "stdin"}
                : new String[] {mode};
        for (String m : modes) {
            run(m, slack);
            System.out.println(m + ": survived");
        }
        System.out.println("done");
    }

    static void run(String m, int slack) {
        badResult = false;
        layoutResult = null;
        String outcome;
        Throwable failure = null;
        try {
            fillHeap(slack);
            outcome = act(m);
        } catch (OutOfMemoryError e) {
            fill = null;
            outcome = "OOME caught";
        } catch (Throwable t) {
            fill = null;
            failure = t;
            outcome = null;
        }
        fill = null;
        System.gc();
        // Record a wrong value without allocating while the heap is full.
        // Creating AssertionError there could itself OOME and hide the error.
        if (badResult) throw new AssertionError("wrong successful result in " + m);
        if (layoutResult != null) {
            // Reflect only after recovery, preserving ValueLayout's cold first
            // use and keeping assertion/report allocations outside pressure.
            try {
                Class<?> c = Class.forName("java.lang.foreign.ValueLayout");
                Object carrier = c.getMethod("carrier").invoke(layoutResult);
                long size = ((Number) c.getMethod("byteSize").invoke(layoutResult)).longValue();
                long alignment = ((Number) c.getMethod("byteAlignment").invoke(layoutResult)).longValue();
                Object order = c.getMethod("order").invoke(layoutResult);
                if (carrier != int.class || size != 4 || alignment != 4
                        || order != java.nio.ByteOrder.nativeOrder())
                    throw new AssertionError("wrong ValueLayout fields after recovery");
            } catch (ReflectiveOperationException e) { throw new AssertionError(e); }
            layoutResult = null;
        }
        if (failure != null) {
            // JDK Utils.<clinit>, for example, explicitly wraps OOME in EIIE.
            // Reject unrelated failures rather than treating every Throwable
            // as proof that the allocation path recovered.
            Throwable cause = failure;
            for (int depth = 0; depth < 16 && !(cause instanceof OutOfMemoryError); depth++) {
                Throwable next = cause.getCause();
                if (next == null || next == cause) break;
                cause = next;
            }
            if (!(cause instanceof OutOfMemoryError))
                throw new AssertionError("unexpected pressure failure in " + m, failure);
            outcome = "threw " + failure.getClass().getName();
        }
        System.err.println(formatOutcome(m, slack, outcome));
    }

    static String formatOutcome(String mode, int slack, String outcome) {
        return "[zr4ab] " + mode + " slack=" + slack + ": " + outcome;
    }

    /** Fill to the ceiling, then free {@code slack} KiB from the tail. */
    static void fillHeap(int slack) {
        fill = new ArrayList<>(1 << 20);
        try {
            while (true) {
                fill.add(new byte[64 * 1024]);
            }
        } catch (OutOfMemoryError e) {
            // Big chunks no longer fit; top up with small ones.
        }
        try {
            while (true) {
                fill.add(new byte[1024]);
            }
        } catch (OutOfMemoryError e) {
            // Full.
        }
        for (int i = 0; i < slack && !fill.isEmpty(); i++) {
            fill.remove(fill.size() - 1);
        }
    }

    static String act(String m) throws Exception {
        switch (m) {
            case "layout": {
                Class<?> c = Class.forName("java.lang.foreign.ValueLayout");
                Object f = c.getField("JAVA_INT").get(null);
                if (f == null) badResult = true;
                layoutResult = f;
                return f != null ? "fitted" : "fitted (null)";
            }
            case "bigint": {
                java.math.BigInteger b = java.math.BigInteger.valueOf(12345L);
                if (b.signum() != 1 || b.longValue() != 12345L
                        || java.math.BigInteger.ZERO.longValue() != 0
                        || java.math.BigInteger.TEN.longValue() != 10) badResult = true;
                return b.signum() == 1 ? "fitted" : "fitted (bad signum)";
            }
            case "unsafe": {
                Class<?> c = Class.forName("sun.misc.Unsafe");
                if (c == null) badResult = true;
                return c != null ? "fitted" : "fitted (null)";
            }
            case "stdin": {
                java.io.InputStream in = System.in;
                if (in == null) badResult = true;
                return in != null ? "fitted" : "fitted (null)";
            }
            case "forname": {
                String[] names = {
                    "java.util.concurrent.Exchanger", "java.util.concurrent.Phaser",
                    "java.util.zip.Adler32", "java.util.zip.CRC32C",
                    "java.text.Bidi", "java.util.StringJoiner",
                    "java.util.concurrent.LinkedTransferQueue",
                    "java.util.concurrent.atomic.LongAccumulator",
                    "java.util.concurrent.atomic.DoubleAdder",
                    "java.time.chrono.HijrahDate", "java.util.Spliterators",
                    "java.util.PriorityQueue", "java.util.IdentityHashMap",
                    "java.util.EnumMap", "java.util.BitSet",
                };
                int loaded = 0;
                for (String n : names) {
                    if (Class.forName(n) != null) {
                        loaded++;
                    }
                }
                if (loaded != names.length) badResult = true;
                return "fitted " + loaded;
            }
            case "arrays": {
                Class<?> c = Zr4AbNoAbort.class;
                for (int d = 0; d < 200; d++) {
                    c = Array.newInstance(c, 0).getClass();
                }
                String name = c.getName();
                if (name.length() != 214 || !name.endsWith("LZr4AbNoAbort;")) badResult = true;
                return "fitted " + name.length();
            }
            default:
                throw new IllegalArgumentException(m);
        }
    }
}
