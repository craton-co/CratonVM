// bench/jetty12/fixture/Main.java
// WP8.7 placeholder fixture for Eclipse Jetty 12 forcing-function smoke.
//
// Real Jetty boot is staged from $JETTY12_HOME/bin/jetty.sh start when present.
// Placeholder probes: virtual thread spawn (Jetty 12 main new API surface).
// Today's baseline pins the WP0.1 println NPE failure mode
// (memory/finding_println_regression.md). When WP0.1 lands and the rc flips
// to 0, bench-baseline.json should be updated in the same PR.
public class Main {
    public static void main(String[] args) throws Exception {
        System.out.println("jetty12_fixture: starting baseline smoke");
        System.out.println("jetty12_fixture: virtual thread spawn (Jetty 12 main new API surface) probe");
        Thread t = Thread.ofVirtual().unstarted(() -> {});
        t.start();
        t.join();
        System.out.println("jetty12_fixture: ok");
    }
}
