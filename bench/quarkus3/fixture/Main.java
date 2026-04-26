// bench/quarkus3/fixture/Main.java
// WP8.7 placeholder fixture for Quarkus 3 dev mode forcing-function smoke.
//
// Real fixture would invoke `mvn quarkus:dev` against a tiny example project — handled by maven matrix slot.
// Placeholder probes: MethodHandle.invokeExact (Arc DI).
// Today's baseline pins the WP0.1 println NPE failure mode
// (memory/finding_println_regression.md). When WP0.1 lands and the rc flips
// to 0, bench-baseline.json should be updated in the same PR.
public class Main {
    public static void main(String[] args) throws Throwable {
        System.out.println("quarkus3_fixture: starting baseline smoke");
        System.out.println("quarkus3_fixture: MethodHandle.invokeExact (Arc DI) probe");
        java.lang.invoke.MethodHandle mh = java.lang.invoke.MethodHandles.lookup().findStatic(
            Integer.class, "parseInt", java.lang.invoke.MethodType.methodType(int.class, String.class));
        int v = (int) mh.invokeExact("42");
        if (v != 42) throw new AssertionError("mh result wrong");
        System.out.println("quarkus3_fixture: ok");
    }
}
