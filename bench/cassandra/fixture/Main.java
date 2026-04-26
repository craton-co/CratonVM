// bench/cassandra/fixture/Main.java
// WP8.7 placeholder fixture for Apache Cassandra 5 forcing-function smoke.
//
// Real Cassandra smoke runs $CASSANDRA_HOME/bin/cassandra -f against cassandra.yaml.
// Placeholder probes: VarHandle volatile (off-heap memtable atomics).
// Today's baseline pins the WP0.1 println NPE failure mode
// (memory/finding_println_regression.md). When WP0.1 lands and the rc flips
// to 0, bench-baseline.json should be updated in the same PR.
public class Main {
    public static void main(String[] args) throws Exception {
        System.out.println("cassandra_fixture: starting baseline smoke");
        System.out.println("cassandra_fixture: VarHandle volatile (off-heap memtable atomics) probe");
        java.lang.invoke.VarHandle vh = java.lang.invoke.MethodHandles.arrayElementVarHandle(int[].class);
        int[] arr = new int[1];
        vh.setVolatile(arr, 0, 7);
        if ((int) vh.getVolatile(arr, 0) != 7) throw new AssertionError("vh roundtrip");
        System.out.println("cassandra_fixture: ok");
    }
}
