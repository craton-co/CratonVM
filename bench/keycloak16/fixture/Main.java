// bench/keycloak16/fixture/Main.java
// WP8.7 placeholder fixture for Keycloak 16 forcing-function smoke.
//
// Real Keycloak 16 boot is staged from $KEYCLOAK16_HOME/bin/standalone.sh
// when present. When absent, this placeholder probes the API surface most-
// stressed by Keycloak boot: ConcurrentHashMap.computeIfAbsent + sequential
// System.out.println (the WP0.1 regression site). Today's baseline pins
// the EXISTING failure mode. Once WP0.1 lands the rc flips to 0 and
// bench-baseline.json should be updated in the same PR.
public class Main {
    public static void main(String[] args) throws Exception {
        System.out.println("keycloak16_fixture: starting baseline smoke");
        System.out.println("keycloak16_fixture: chm computeIfAbsent probe");
        java.util.concurrent.ConcurrentHashMap<String,String> m = new java.util.concurrent.ConcurrentHashMap<>();
        m.computeIfAbsent("k", k -> "v");
        System.out.println("keycloak16_fixture: ok");
    }
}
