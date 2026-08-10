/**
 * Isolates whether the [moving-young] fallback reason
 * `innermost-rbp-belongs-to-unguarded-callee` tracks INDIRECT calls.
 *
 * `innermost_frame_method` identifies the innermost JIT frame's owner by
 * decoding the five-byte `E8 rel32` before the saved return address. A frame
 * entered by an indirect call has no such encoding, so resolution fails closed
 * and the collection falls back to the non-moving sweep.
 *
 * Three arms, identical allocation and identical recursion depth -- only the
 * CALL FORM differs:
 *
 *   static  -- a recursive static method. invokestatic to a known target is the
 *              direct `E8 rel32` form. This is the real control; the earlier
 *              "monomorphic" attempt used an interface-typed field, so it was
 *              invokeinterface and indirect regardless of receiver count.
 *   virtual -- one final receiver type through a class-typed field.
 *   iface   -- five receiver types through an interface-typed field, which is
 *              megamorphic and cannot be inline-cached to a direct call.
 *
 * If the fallback count is low for `static` and high for `iface`, the
 * indirect-call reading is confirmed. If all three are the same, it is not the
 * call form and the decode is failing for another reason.
 */
public class MovingYoungFallbackCallFormProbe {

    interface Node { long walk(int d); }

    static final class One implements Node {
        One next;
        Node inext;
        public long walk(int d) {
            if (d <= 0) { byte[] j = new byte[512]; j[0] = 1; return j.length; }
            return next.walk(d - 1) + 1;      // virtual, final receiver
        }
        long ifaceWalk(int d) {
            if (d <= 0) { byte[] j = new byte[512]; j[0] = 1; return j.length; }
            return ((One) null) == null && inext != null ? inext.walk(d - 1) + 1 : 0;
        }
    }

    static final class Two implements Node {
        Node inext;
        public long walk(int d) {
            if (d <= 0) { byte[] j = new byte[512]; j[0] = 2; return j.length; }
            return inext.walk(d - 1) + 2;     // interface, megamorphic
        }
    }
    static final class Three implements Node {
        Node inext;
        public long walk(int d) {
            if (d <= 0) { byte[] j = new byte[512]; j[0] = 3; return j.length; }
            return inext.walk(d - 1) + 3;
        }
    }
    static final class Four implements Node {
        Node inext;
        public long walk(int d) {
            if (d <= 0) { byte[] j = new byte[512]; j[0] = 4; return j.length; }
            return inext.walk(d - 1) + 4;
        }
    }
    static final class Five implements Node {
        Node inext;
        public long walk(int d) {
            if (d <= 0) { byte[] j = new byte[512]; j[0] = 5; return j.length; }
            return inext.walk(d - 1) + 5;
        }
    }

    /** Recursive STATIC call — the direct `E8 rel32` form. */
    static long staticWalk(int d) {
        if (d <= 0) { byte[] j = new byte[512]; j[0] = 9; return j.length; }
        return staticWalk(d - 1) + 1;
    }

    static One virtualChain(int len) {
        One head = new One();
        One cur = head;
        for (int i = 1; i < len; i++) { One n = new One(); cur.next = n; cur = n; }
        cur.next = head;
        return head;
    }

    /**
     * Four receiver types, all of which follow `inext`. `One` is deliberately
     * EXCLUDED: its `walk` follows the class-typed `next` field that only the
     * virtual arm populates, so including it here walked into a null and the
     * arm died with an NPE before recording a single iteration — an arm that
     * produces no output reads just like an arm with no fallbacks.
     */
    static Node ifaceChain(int len) {
        Node[] all = new Node[len];
        for (int i = 0; i < len; i++) {
            switch (i % 4) {
                case 0:  all[i] = new Two();   break;
                case 1:  all[i] = new Three(); break;
                case 2:  all[i] = new Four();  break;
                default: all[i] = new Five();  break;
            }
        }
        for (int i = 0; i < len; i++) {
            Node n = all[(i + 1) % len];
            Node c = all[i];
            if (c instanceof Two)        ((Two) c).inext = n;
            else if (c instanceof Three) ((Three) c).inext = n;
            else if (c instanceof Four)  ((Four) c).inext = n;
            else                         ((Five) c).inext = n;
        }
        return all[0];
    }

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "static";
        int seconds = args.length > 1 ? Integer.parseInt(args[1]) : 25;
        int depth = args.length > 2 ? Integer.parseInt(args[2]) : 40;

        One vchain = mode.equals("virtual") ? virtualChain(depth + 2) : null;
        Node ichain = mode.equals("iface") ? ifaceChain(depth + 2) : null;

        long deadline = System.currentTimeMillis() + seconds * 1000L;
        long iters = 0, acc = 0;
        while (System.currentTimeMillis() < deadline) {
            for (int i = 0; i < 200; i++) {
                if (mode.equals("static"))      acc += staticWalk(depth);
                else if (mode.equals("virtual")) acc += vchain.walk(depth);
                else                             acc += ichain.walk(depth);
                iters++;
            }
        }
        System.out.println("PROBE mode=" + mode + " depth=" + depth
                + " iters=" + iters + " acc=" + acc);
    }
}
