// -Xmx32m: a compiled fill row splices new/constructor under catch(IAE).
// Allocation failure must remain an OutOfMemoryError, with no handler replay.
public class J27CoveredAllocationFailure {
    static final Box[] HELD = new Box[1_000_000];
    static byte[] reserve = new byte[1_048_576];
    static final class Box {
        long a, b, c, d;
        Box(int v) {
            if (v < 0) throw new IllegalArgumentException("negative");
            a = v; b = v + 1; c = v + 2; d = v + 3;
        }
    }
    static Box make(int v) {
        try { return new Box(v); }
        catch (IllegalArgumentException e) { throw new IndexOutOfBoundsException("wrong handler"); }
    }
    static void fill(int n) {
        for (int i = 0; i < n; i++) HELD[i] = make(i);
    }
    public static void main(String[] args) {
        for (int r = 0; r < 30; r++) fill(10000);
        System.out.println("new-warm ok");
        Throwable failure = null;
        try { fill(HELD.length); }
        catch (Throwable e) { failure = e; }
        reserve = null;
        System.gc();
        System.out.println("new-oom " + (failure == null ? "missing"
            : failure.getClass().getName() + " " + failure.getMessage()));
    }
}
