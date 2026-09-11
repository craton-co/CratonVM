import java.lang.reflect.Field;
import jdk.internal.misc.Unsafe;

/**
 * L5 -- the exact `Unsafe` surface `ForkJoinPool$WorkQueue` runs on.
 *
 * Reached with (on `javac` AND on `java`, both VMs):
 *
 *   --add-exports java.base/jdk.internal.misc=ALL-UNNAMED
 *
 * Check the `rows` trailer (11) before believing a clean diff.
 *
 * # Why this exists as a separate probe
 *
 * With the real `ForkJoinPool` bytecode running, an externally submitted task
 * is executed twice — once by the submitting thread's help path and once by a
 * worker. The obvious suspect is the primitive underneath: the queue is a
 * handful of plain `int` fields (`base`, `top`, `phase`, `source`) read and
 * written through `Unsafe`'s opaque/acquire/release accessors, plus a slot
 * array written with `putReferenceRelease` and claimed with a CAS.
 *
 * This probe asks that surface directly, so the answer is a measurement rather
 * than a reading:
 *
 *   * five ADJACENT `int` fields get five DISTINCT offsets, and a write
 *     through one is not visible through another (the aliasing failure);
 *   * every value written through `Unsafe` is visible to plain bytecode and
 *     vice versa (the two-address-spaces failure);
 *   * `getAndBitwiseOrInt` — which is what `ForkJoinTask.setDone` uses —
 *     returns the previous value and ORs the new bit;
 *   * `putReferenceRelease` / `getReferenceAcquire` round-trip at four array
 *     indices, and the plain array read agrees.
 *
 * It prints no offset. `objectFieldOffset` is a VM's own numbering — CratonVM
 * answers a SLOT INDEX where HotSpot answers a byte offset — so printing one
 * makes every row differ for a reason that is not a defect. What is printed is
 * the property that has to hold whatever the numbering is.
 */
public class L5UnsafeAccess {
    static final Unsafe U = Unsafe.getUnsafe();
    static int rows;

    /** The shape of a `ForkJoinPool$WorkQueue`: adjacent ints plus a slot array. */
    static class W {
        int base;
        int top;
        int phase;
        int source;
        volatile int stat;
        Object[] array;
        Object o1;
    }

    static long off(String n) {
        try {
            return U.objectFieldOffset(W.class.getDeclaredField(n));
        } catch (Exception e) {
            return -1L;
        }
    }

    static void say(String s) {
        rows++;
        System.out.println(s);
    }

    public static void main(String[] args) {
        W w = new W();
        long ob = off("base");
        long ot = off("top");
        long op = off("phase");
        long os = off("source");
        long ost = off("stat");
        boolean distinct = ob != ot && ot != op && op != os && os != ost
                && ob != op && ob != os && ob != ost && ot != os && ot != ost && op != ost;
        say("fiveAdjacentIntFieldsHaveDistinctOffsets=" + distinct);

        U.putIntOpaque(w, ob, 11);
        U.putIntOpaque(w, ot, 22);
        U.putIntOpaque(w, op, 33);
        U.putIntVolatile(w, os, 44);
        U.putIntRelease(w, ost, 55);
        say("unsafeWritesSeenByBytecode=" + w.base + "," + w.top + "," + w.phase
                + "," + w.source + "," + w.stat);
        say("unsafeReadsAgree=" + U.getIntOpaque(w, ob) + "," + U.getIntAcquire(w, ot)
                + "," + U.getIntVolatile(w, op) + "," + U.getInt(w, os)
                + "," + U.getIntVolatile(w, ost));

        w.base = 99;
        w.top = 98;
        say("bytecodeWritesSeenByUnsafe=" + U.getIntVolatile(w, ob) + "," + U.getIntVolatile(w, ot));

        // ForkJoinTask.setDone is getAndBitwiseOrInt on `status`.
        W x = new W();
        x.stat = 0;
        int prev = U.getAndBitwiseOrInt(x, ost, 0x40000000);
        say("getAndBitwiseOrInt prev=" + prev + " now=" + x.stat);
        int prev2 = U.getAndBitwiseOrInt(x, ost, 0x40000000);
        say("getAndBitwiseOrIntIdempotent prev=" + prev2 + " now=" + x.stat);

        // WorkQueue.push stores the task with a release store; poll/tryUnpush
        // read and claim the same slot.
        long ab = U.arrayBaseOffset(Object[].class);
        long as = U.arrayIndexScale(Object[].class);
        Object[] arr = new Object[4];
        for (int i = 0; i < 4; i++) {
            U.putReferenceRelease(arr, ab + i * as, "v" + i);
        }
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < 4; i++) {
            sb.append(U.getReferenceAcquire(arr, ab + i * as)).append('/').append(arr[i]).append(' ');
        }
        say("arrayReleaseAcquireRoundTrip=" + sb.toString().trim());

        // The claim itself: CAS the slot from the task to null, then again.
        Object tok = arr[2];
        boolean first = U.compareAndSetReference(arr, ab + 2 * as, tok, null);
        boolean second = U.compareAndSetReference(arr, ab + 2 * as, tok, null);
        say("slotClaimedExactlyOnce first=" + first + " second=" + second
                + " slot=" + arr[2]);

        long ib = U.arrayBaseOffset(int[].class);
        long is = U.arrayIndexScale(int[].class);
        int[] ia = new int[4];
        for (int i = 0; i < 4; i++) {
            U.putIntOpaque(ia, ib + i * is, i + 7);
        }
        say("intArrayOpaqueRoundTrip=" + ia[0] + "," + ia[1] + "," + ia[2] + "," + ia[3]
                + " reads=" + U.getIntOpaque(ia, ib) + "," + U.getIntOpaque(ia, ib + is)
                + "," + U.getIntOpaque(ia, ib + 2 * is) + "," + U.getIntOpaque(ia, ib + 3 * is));

        // A reference field, not an array slot: the other half of the queue.
        W y = new W();
        U.putReferenceRelease(y, U.objectFieldOffset(fieldOf("o1")), "held");
        say("referenceFieldReleaseAcquire=" + U.getReferenceAcquire(y, U.objectFieldOffset(fieldOf("o1")))
                + "/" + y.o1);
        say("referenceFieldCas=" + U.compareAndSetReference(y, U.objectFieldOffset(fieldOf("o1")),
                "held", null) + " now=" + y.o1);

        System.out.println("rows " + rows);
        System.out.println("DONE L5UnsafeAccess");
    }

    static Field fieldOf(String n) {
        try {
            return W.class.getDeclaredField(n);
        } catch (Exception e) {
            throw new AssertionError(e);
        }
    }
}
