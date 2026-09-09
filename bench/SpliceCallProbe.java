/**
 * SpliceCallProbe -- a statically-bound call that SURVIVES a splice.
 *
 * `mid` is spliceable and is spliced into `step`. `pick` is not: it carries a
 * `tableswitch`, which every inline resolver in the tree refuses outright. So
 * the call to `pick` ends up at a COMBINED-BUFFER pc inside a relocated body,
 * which is the one site class `ir_direct_calls` had no row for -- the caller's
 * own scan loop is the only thing that ever filled that map.
 *
 * Without the row the call lowers to `jit_invoke_dispatch`, which resolves the
 * callee by NAME on every execution. `CRATONVM_JIT_IR_SPLICE_DIRECT_CALL=0`
 * restores exactly that, so the two arms are one binary apart.
 *
 * Usage: SpliceCallProbe [reps]     default 4,000,000
 */
public class SpliceCallProbe {
    static int pick(int k) {
        switch (k & 7) {
            case 0: return 3;
            case 1: return 5;
            case 2: return 7;
            case 3: return 11;
            case 4: return 13;
            case 5: return 17;
            case 6: return 19;
            default: return 23;
        }
    }

    // Spliceable: straight-line, one trailing return, no static read, no
    // branch of its own -- and it makes the call that must survive.
    static int mid(int x) {
        return pick(x) + (x >>> 3);
    }

    static int step(int acc, int i) {
        return acc * 31 + mid(acc ^ i);
    }

    public static void main(String[] args) {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 4_000_000;
        int warm = 0;
        for (int i = 0; i < 3_000_000; i++) warm = step(warm, i);
        long t0 = System.nanoTime();
        int acc = 0;
        for (int i = 0; i < reps; i++) acc = step(acc, i);
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        System.out.println("1. splicecall (" + reps + ") : " + ms + " ms  [" + acc + "]");
        if (warm == 0x7FFFFFFF) System.out.println(warm);
    }
}
