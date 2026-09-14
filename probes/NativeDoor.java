// Prices an interpreted call whose target is a REGISTERED NATIVE against two
// bytecode calls of the same shape, in one process.
//
// Why this probe exists. `probes/Dispatch.java` prices the four invoke kinds,
// but every arm of it reaches a bytecode callee and is therefore served by one
// of the three fast doors. The doors DECLINE a registered native -- 69% of
// every decline on a collator workload, by the 2026-09-08 census -- and what
// serves it instead is the general dispatcher's `Native` / `VirtualNative`
// arm. That arm is what this probe measures.
//
// The controls are the point. `bcStatic` and `bcVirtual` call ordinary
// bytecode methods of this class with the same argument shapes, so they are
// served by the doors and cannot be touched by any change to the cached-native
// arms; `nocall` pushes no frame at all. An arm that moves while all three
// controls sit still is the change. All arms are interleaved in both
// directions on alternating rounds and reported as min-of-N.
//
//   javac -d /tmp/probe probes/NativeDoor.java
//   cratonvm --java-home <JDK 25> --nojit -c /tmp/probe NativeDoor 400000 7
//   java -Xint -cp /tmp/probe NativeDoor 400000 7      # the reference column
//
// `CRATONVM_DBG_FIELD_SITE=1` prints `cached-native: facts_static=...
// facts_virtual=... resolve=... leaf=...` beside the door census. READ THAT
// FIRST: an arm whose calls never reach a cached-native entry is measuring
// something else, and the clock cannot say which.
public class NativeDoor {
    static int sink;
    static Object osink;
    static final Object OBJ = new Object();
    static final String S = "abcdefghij";
    static final int[] SRC = new int[8];
    static final int[] DST = new int[8];
    static final StringBuilder SB = new StringBuilder("abcdefghij");

    // The two bytecode controls: same arity, same return kind, reached
    // through a door.
    static int bcS1(int a) { return a; }
    int bcV1(int a) { return a; }

    static int cNone(int n) { int s = 0; for (int i = 0; i < n; i++) s += i; return s; }
    static int cBcS1(int n) { int s = 0; for (int i = 0; i < n; i++) s += bcS1(i); return s; }
    static int cBcV1(int n, NativeDoor o) { int s = 0; for (int i = 0; i < n; i++) s += o.bcV1(i); return s; }

    static int cObjHash(int n, Object o) { int s = 0; for (int i = 0; i < n; i++) s += o.hashCode(); return s; }
    static int cGetClass(int n, Object o) { int s = 0; for (int i = 0; i < n; i++) s += o.getClass() == null ? 1 : 0; return s; }
    static int cIdHash(int n, Object o) { int s = 0; for (int i = 0; i < n; i++) s += System.identityHashCode(o); return s; }
    static int cStrCharAt(int n, String x) { int s = 0; for (int i = 0; i < n; i++) s += x.charAt(i & 7); return s; }
    static int cStrHash(int n, String x) { int s = 0; for (int i = 0; i < n; i++) s += x.hashCode(); return s; }
    static int cArrayCopy(int n) { int s = 0; for (int i = 0; i < n; i++) { System.arraycopy(SRC, 0, DST, 0, 4); s += DST[0]; } return s; }
    static int cReqNonNull(int n, Object o) { int s = 0; for (int i = 0; i < n; i++) s += java.util.Objects.requireNonNull(o) == null ? 1 : 0; return s; }
    static int cNanoTime(int n) { int s = 0; for (int i = 0; i < n; i++) s += (int) System.nanoTime(); return s; }
    // `StringBuilder`'s accessors are registered natives in this VM
    // (`is_string_builder_layout_native_override`), so these reach the
    // `VirtualNative` inline-cache arm — the largest cached-native population
    // there is, and the one the doors decline most often.
    static int cSbLength(int n, StringBuilder b) { int s = 0; for (int i = 0; i < n; i++) s += b.length(); return s; }
    static int cSbCharAt(int n, StringBuilder b) { int s = 0; for (int i = 0; i < n; i++) s += b.charAt(i & 7); return s; }

    public static void main(String[] a) {
        int n = a.length > 0 ? Integer.parseInt(a[0]) : 400000;
        int rounds = a.length > 1 ? Integer.parseInt(a[1]) : 7;
        // A third argument runs ONE arm, which is what makes the process-wide
        // `cached-native:` census readable per arm: without it every run mixes
        // thirteen arms and the counter cannot say which of them reached a
        // cached-native entry at all. Same reason `probes/FrameShape.java`
        // takes one.
        String only = a.length > 2 ? a[2] : null;
        NativeDoor o = new NativeDoor();
        String[] nm = {
            "nocall", "bcStatic", "bcVirtual",
            "objHashCode", "objGetClass", "identityHashCode",
            "strCharAt", "strHashCode", "arraycopy", "requireNonNull", "nanoTime",
            "sbLength", "sbCharAt",
        };
        double[] m = new double[nm.length];
        for (int i = 0; i < m.length; i++) m[i] = 1e18;
        int sum = 0;
        long t;
        for (int r = 0; r < rounds; r++) {
            boolean fwd = (r & 1) == 0;
            for (int k = 0; k < nm.length; k++) {
                int j = fwd ? k : nm.length - 1 - k;
                if (only != null && !only.equals(nm[j])) {
                    continue;
                }
                t = System.nanoTime();
                switch (j) {
                    case 0:  sum += cNone(n); break;
                    case 1:  sum += cBcS1(n); break;
                    case 2:  sum += cBcV1(n, o); break;
                    case 3:  sum += cObjHash(n, OBJ); break;
                    case 4:  sum += cGetClass(n, OBJ); break;
                    case 5:  sum += cIdHash(n, OBJ); break;
                    case 6:  sum += cStrCharAt(n, S); break;
                    case 7:  sum += cStrHash(n, S); break;
                    case 8:  sum += cArrayCopy(n); break;
                    case 9:  sum += cReqNonNull(n, OBJ); break;
                    case 10: sum += cNanoTime(n); break;
                    case 11: sum += cSbLength(n, SB); break;
                    default: sum += cSbCharAt(n, SB); break;
                }
                double d = (System.nanoTime() - t) / (double) n;
                if (d < m[j]) m[j] = d;
            }
        }
        for (int i = 0; i < nm.length; i++) {
            if (only != null && !only.equals(nm[i])) {
                continue;
            }
            System.out.println(nm[i] + "\t" + m[i] + "\tdelta=" + (m[i] - m[0]));
        }
        sink = sum;
        osink = o;
        if (sink == 42 && osink == null) System.out.println("x");
    }
}
