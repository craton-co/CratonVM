// bench/keycloak26/fixture/Main.java
// WP8.7 placeholder fixture for Keycloak 26 (Quarkus-based) forcing-function smoke.
//
// Real Keycloak 26 boot is staged from $KEYCLOAK26_HOME/bin/kc.sh when present.
// Placeholder probes: invokedynamic lambda metafactory.
// Today's baseline pins the WP0.1 println NPE failure mode
// (memory/finding_println_regression.md). When WP0.1 lands and the rc flips
// to 0, bench-baseline.json should be updated in the same PR.
public class Main {
    public static void main(String[] args) throws Exception {
        System.out.println("keycloak26_fixture: starting baseline smoke");
        System.out.println("keycloak26_fixture: invokedynamic lambda metafactory probe");
        Runnable r = () -> { /* lambda body */ };
        r.run();
        System.out.println("keycloak26_fixture: ok");
    }
}
