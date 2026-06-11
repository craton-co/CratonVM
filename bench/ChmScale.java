// Recreation of the (deleted) apps/chm_basic/ChmScale W2-CHM reproducer, per
// the contract pinned in vm/tests/wave2_chm.rs and the is_known_miscompile
// W2-CHM comment: put/get N keys "k0".."k(N-1)" -> Integer.valueOf(i) into a
// ConcurrentHashMap across sizes 16..1000, crossing both the OSR back-edge
// threshold (1000) and the per-callee invocation threshold (2000) in the same
// outer frame. Expected (HotSpot-identical): every entry round-trips,
// firstMiss=-1. The historical miscompile returned value=0 Integers for
// k992..k999.
import java.util.concurrent.ConcurrentHashMap;

public final class ChmScale {
    public static void main(String[] args) {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 5;
        for (int r = 0; r < reps; r++) {
            for (int size : new int[] {16, 32, 64, 128, 256, 512, 1000}) {
                ConcurrentHashMap<String, Integer> map = new ConcurrentHashMap<>();
                for (int i = 0; i < size; i++) {
                    map.put("k" + i, Integer.valueOf(i));
                }
                int found = 0;
                int firstMiss = -1;
                for (int i = 0; i < size; i++) {
                    Integer v = map.get("k" + i);
                    if (v != null && v.intValue() == i) {
                        found++;
                    } else if (firstMiss == -1) {
                        firstMiss = i;
                    }
                }
                if (r == reps - 1) {
                    System.out.println("size=" + size + " mapSize=" + map.size()
                            + " found=" + found + " firstMiss=" + firstMiss);
                }
            }
        }
    }
}
