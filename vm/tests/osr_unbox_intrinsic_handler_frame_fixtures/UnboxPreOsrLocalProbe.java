// An inline intrinsic that CLAIMS an `invokevirtual` inside a protected range
// must still deliver that site's NPE to the method's own handler, with the
// handler's locals live.
//
// # What this guards
//
// `compile_osr_artifact` admits a method with a non-empty exception table on one
// promise (RBC.6b): every throwing site inside a protected range publishes a
// reason-9 (`PendingException`) precise frame, so
// `route_osr_exception_out_of_artifact` may deduce that NO stashed frame means
// the throw was outside every protected range and propagate. The predicate that
// checks it, `first_unsupported_precise_frame_site`, reads the BYTECODE, and
// clears opcode 0xb6 because `precise_frame_publishing_opcode` says the ORDINARY
// invokevirtual lowering publishes.
//
// The BOX_UNBOX region then substitutes its own lowering for `Integer.intValue`
// / `Long.longValue`, whose null-receiver edge is a reason-6
// (`ReceiverTypeChanged`) deopt stub rather than a reason-9 publication. That is
// sound today for a reason worth stating, because it is not obvious and nothing
// else in the tree records it: a reason-6 deopt is not a weaker publication, it
// is a STRONGER action. It abandons the compiled frame and hands a reconstructed
// one back to the interpreter, which raises the NPE and searches the exception
// table itself. The reason-9 frame is for an exception arriving INSIDE the
// compiled body that must be routed without leaving it.
//
// This probe pins that. If the intrinsic ever grows an edge that stays in
// compiled code without publishing, the handler is skipped or entered on stale
// locals, and this goes red.
// `internal/fixed-bugs/jit-superseded-implicit-npe-leak-FIXED-20260903.md`
// records the reading that was retracted to get here.
//
// # Four properties are load-bearing — do not "tidy" them
//
//  1. **The unbox is the ONLY throwing opcode inside the `try`.** Not tidiness:
//     `putstatic` (0xb3), array loads and `ldc` are absent from
//     `precise_frame_publishing_opcode`'s admitted set, so ONE of them inside the
//     protected range refuses the whole method for OSR and this probe measures
//     nothing while still printing PASS. Everything in the loop is a local; the
//     statics are written once, after it.
//  2. **`arm()` is invoked once.** OSR is then the only compile door, which is
//     the door whose admission is under test.
//  3. **The interrogated locals are set BEFORE the loop.** They are the stale
//     pre-OSR ones `route_osr_exception_out_of_artifact` names as the hazard:
//     "the live frame's locals are the stale pre-OSR ones the compiled code never
//     advanced. Entering a handler on those is a silent wrong answer."
//  4. **`witness` is ADVANCED by the loop.** A stale read and a correct read have
//     to differ, or the check cannot fail. `witness == 7` is the entry value and
//     is rejected explicitly.
//
// HotSpot (the oracle): caught=400 bad=0 drift=0 escaped=0 sum=6486400.
public class UnboxPreOsrLocalProbe {

    static final int N = 200_000;
    static final Integer[] BOXES = new Integer[64];
    static final String[] TAGS = {"a", "b", "c", "d", "e", "f", "g", "h"};

    static {
        for (int i = 0; i < 64; i++) {
            BOXES[i] = Integer.valueOf(i + 1);
        }
    }

    static int caught;
    static int bad;
    static int drift;
    static long sum;

    static void arm() {
        final long marker = 0xCAFEBABEL;   // pre-OSR, never written again
        final String pre = TAGS[3];        // pre-OSR reference
        long witness = 7;                  // pre-OSR, ADVANCED by the loop
        long s = 0;
        int c = 0;
        int b = 0;
        int d = 0;
        for (int i = 0; i < N; i++) {
            witness = witness * 3 + 1;
            Integer box = (i % 500 == 499) ? null : BOXES[i & 63];
            long expect = witness;
            try {
                s += box.intValue();
            } catch (NullPointerException e) {
                c++;
                if (marker != 0xCAFEBABEL) {
                    b++;
                }
                if (pre != TAGS[3]) {
                    b++;
                }
                // A stale pre-OSR read is 7, or a value from an earlier
                // iteration; only the advanced one matches.
                if (witness != expect || witness == 7) {
                    d++;
                }
            }
        }
        sum += s;
        caught += c;
        bad += b;
        drift += d;
    }

    public static void main(String[] args) {
        int escaped = 0;
        try {
            arm();
        } catch (Throwable t) {
            escaped++;
            System.out.println("ESCAPED arm: " + t.getClass().getName());
        }
        System.out.println("caught=" + caught + " bad=" + bad + " drift=" + drift
                + " escaped=" + escaped + " sum=" + sum);
        System.out.println(caught == 400 && bad == 0 && drift == 0 && escaped == 0
                ? "PASS UnboxPreOsrLocalProbe"
                : "FAIL UnboxPreOsrLocalProbe");
    }
}
