// bench/ejbca/fixture/Main.java
// WP8.7 placeholder fixture for EJBCA CE 9 (broader path than bench/wildfly/) forcing-function smoke.
//
// Distinct from bench/wildfly/ which pins cesecore-common DirectRunner subset.
// Placeholder probes: JCA provider list (EJBCA boot path).
// Today's baseline pins the WP0.1 println NPE failure mode
// (memory/finding_println_regression.md). When WP0.1 lands and the rc flips
// to 0, bench-baseline.json should be updated in the same PR.
public class Main {
    public static void main(String[] args) throws Exception {
        System.out.println("ejbca_fixture: starting baseline smoke");
        System.out.println("ejbca_fixture: JCA provider list (EJBCA boot path) probe");
        java.security.Security.getProviders();
        System.out.println("ejbca_fixture: ok");
    }
}
