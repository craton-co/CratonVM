// bench/springboot3/fixture/Main.java
// WP8.7 placeholder fixture for Spring Boot 3 (Petclinic) forcing-function smoke.
//
// Real boot is staged from spring-petclinic-3.x.jar in a known location.
// Placeholder probes: JDK Proxy (Spring AOP).
// Today's baseline pins the WP0.1 println NPE failure mode
// (memory/finding_println_regression.md). When WP0.1 lands and the rc flips
// to 0, bench-baseline.json should be updated in the same PR.
public class Main {
    public static void main(String[] args) throws Exception {
        System.out.println("springboot3_fixture: starting baseline smoke");
        System.out.println("springboot3_fixture: JDK Proxy (Spring AOP) probe");
        Object p = java.lang.reflect.Proxy.newProxyInstance(
            Main.class.getClassLoader(),
            new Class<?>[]{ Runnable.class },
            (proxy, method, margs) -> null);
        ((Runnable) p).run();
        System.out.println("springboot3_fixture: ok");
    }
}
