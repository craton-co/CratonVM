// A/B instrument for CRATONVM_JIT_NO_FIELD_ADDR_ELIDE.
//
// The quickened getfield arm used to probe the ZGC object-start bitmap at the
// RECEIVER's address. One bit per 8 arena bytes, so consecutive receivers that
// are far apart in the heap hit different bitmap words. This probe makes that
// the only thing that varies:
//
//   * `iters`, the index array and every executed bytecode are IDENTICAL for
//     all live-set sizes — `idx` is always LEN entries and the loop masks with
//     `i & (LEN-1)`, so there is no `irem` and no per-size shape change.
//   * only `objs.length` changes, which changes how far apart the receivers
//     are and therefore whether the bitmap word is resident.
//
// `refWalk` performs the same aaload and never touches the object, so it is
// the arm the switch cannot reach: it must not move between switch states.
public class AddrProbe {
    static final int LEN = 1 << 16;
    static class Node { int f; Node(int f){this.f=f;} }

    static int fieldWalk(int iters, Node[] objs, int[] idx) {
        int s = 0;
        for (int i = 0; i < iters; i++) s += objs[idx[i & (LEN-1)]].f;
        return s;
    }
    static int refWalk(int iters, Node[] objs, int[] idx) {
        int s = 0;
        for (int i = 0; i < iters; i++) if (objs[idx[i & (LEN-1)]] != null) s++;
        return s;
    }

    public static void main(String[] a) {
        int iters  = a.length > 0 ? Integer.parseInt(a[0]) : 2000000;
        int rounds = a.length > 1 ? Integer.parseInt(a[1]) : 5;
        int[] sizes = {4096, 262144, 2097152};
        int sink = 0;
        System.out.println("liveObjects\trefWalk_ns\tfieldWalk_ns");
        for (int n : sizes) {
            Node[] objs = new Node[n];
            for (int k = 0; k < n; k++) objs[k] = new Node(k);
            int[] idx = new int[LEN];
            long x = 88172645463325252L;                 // xorshift64, deterministic
            for (int k = 0; k < LEN; k++) {
                x ^= x << 13; x ^= x >>> 7; x ^= x << 17;
                idx[k] = (int)((x >>> 1) % n);
            }
            double mr = 1e18, mf = 1e18; long t;
            for (int r = 0; r < rounds; r++) {
                if ((r & 1) == 0) {
                    t=System.nanoTime(); sink+=refWalk(iters,objs,idx);   mr=Math.min(mr,(System.nanoTime()-t)/(double)iters);
                    t=System.nanoTime(); sink+=fieldWalk(iters,objs,idx); mf=Math.min(mf,(System.nanoTime()-t)/(double)iters);
                } else {
                    t=System.nanoTime(); sink+=fieldWalk(iters,objs,idx); mf=Math.min(mf,(System.nanoTime()-t)/(double)iters);
                    t=System.nanoTime(); sink+=refWalk(iters,objs,idx);   mr=Math.min(mr,(System.nanoTime()-t)/(double)iters);
                }
            }
            System.out.println(n+"\t"+r2(mr)+"\t"+r2(mf));
        }
        if (sink == 42) System.out.println("x");
    }
    static double r2(double d){ return Math.round(d*100)/100.0; }
}
