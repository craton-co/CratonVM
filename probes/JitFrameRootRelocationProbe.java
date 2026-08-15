import java.util.ArrayList;
import java.util.Collections;
import java.util.Iterator;
import java.util.List;

/**
 * A moving collector must not relocate an object whose only live reference is a
 * conservatively-scanned JIT frame slot.
 *
 * <p>The slot cannot be rewritten — a conservative scan is over-approximate by
 * construction, so the collector does not know it IS a slot — which is why
 * {@code gc_quiescence::pinned_jit_roots_snapshot()} exists and why every
 * backend that relocates has to consume it. ZGC gained that consumer on
 * 2026-08-14 and had no producer: all four publish sites were still gated on
 * {@code is_g1()}, so the snapshot was empty on every ZGC cycle and the pin
 * looked implemented while pinning nothing.
 *
 * <p>This is what that looks like from Java. {@code Holder}'s {@code this} is
 * live only inside its own compiled {@code <init>} frame between the
 * {@code invokestatic} and the {@code putfield}; the slide moves it, the frame
 * slot keeps the old address, and the {@code putfield} lands in the span
 * {@code compact_low_to} has just zeroed. The caller keeps the relocated copy,
 * whose {@code final} field was therefore never written:
 *
 * <pre>
 *   nullFields   = 1
 *   first bad at = 10191      (deterministic — it is a compile threshold)
 * </pre>
 *
 * <p>One object in 200 000, silently, with no exception anywhere near the
 * defect. {@code UnmodifiableListIteratorJitProbe} is the same bug reaching an
 * application: it dereferences that null and reports an NPE attributed to a
 * loop header.
 *
 * <p>The levers, each of which alone takes it to zero, are what identify the
 * mechanism: {@code CRATONVM_ZGC_RELOCATE=0} (nothing moves),
 * {@code -XX:+UseG1GC} (has the producers), {@code CRATONVM_JIT_OSR=0} (the
 * caller stays interpreted, so the callee is never compiled), and
 * {@code CRATONVM_JIT_DENY=…$Holder.<init>}. In the other direction
 * {@code CRATONVM_DBG_GC_STRESS=1048576} takes it to 67.
 *
 * <p>PASS is {@code nullFields = 0} with a non-zero {@code iterations}; HotSpot
 * JDK 25 prints all zeros.
 */
public class JitFrameRootRelocationProbe {

    static final class Holder {

        private final List<String> delegates;

        /**
         * Left EXACTLY as it is. `CRATONVM_JIT_DENY` on this constructor is one
         * of the levers, so it is the method under test — and the failure is
         * sensitive to its shape: adding so much as a null check inside it
         * changed the compiled body enough to hide the bug while it was still
         * present.
         */
        Holder(List<String> source) {
            List<String> copy = new ArrayList<>(source);
            this.delegates = Collections.unmodifiableList(copy);
        }

        Iterator<String> rawIterator() {
            return this.delegates.iterator();
        }

        /** Observes the field WITHOUT dereferencing it, so a null is a count. */
        Object peekField() {
            return this.delegates;
        }
    }

    public static void main(String[] args) {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 200_000;
        List<String> source = new ArrayList<>();
        source.add("alpha");
        source.add("beta");
        source.add("gamma");

        int nullIterators = 0;
        int nullFields = 0;
        int wrongFieldClass = 0;
        long firstBadAt = -1;
        String firstKind = "-";

        for (int i = 0; i < iterations; i++) {
            Holder h = new Holder(source);
            Object f = h.peekField();
            if (f == null) {
                nullFields++;
                if (firstBadAt < 0) {
                    firstBadAt = i;
                    firstKind = "NULL_FIELD";
                }
                continue;
            }
            if (!(f instanceof List)) {
                wrongFieldClass++;
                if (firstBadAt < 0) {
                    firstBadAt = i;
                    firstKind = "WRONG_CLASS/" + f.getClass().getName();
                }
                continue;
            }
            Iterator<String> it = h.rawIterator();
            if (it == null) {
                nullIterators++;
                if (firstBadAt < 0) {
                    firstBadAt = i;
                    firstKind = "NULL_ITERATOR";
                }
            }
        }

        System.out.println("iterations     = " + iterations);
        System.out.println("nullFields     = " + nullFields);
        System.out.println("wrongFieldCls  = " + wrongFieldClass);
        System.out.println("null iterators = " + nullIterators);
        System.out.println("first bad at   = " + firstBadAt + " kind=" + firstKind);
        boolean ok = nullFields == 0 && wrongFieldClass == 0 && nullIterators == 0;
        System.out.println(ok ? "PROBE PASS" : "PROBE FAIL");
        System.exit(ok ? 0 : 1);
    }
}
