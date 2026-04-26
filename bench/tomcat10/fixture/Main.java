// bench/tomcat10/fixture/Main.java
// WP8.7 placeholder fixture for Apache Tomcat 10 forcing-function smoke.
//
// Real Tomcat boot is staged from $TOMCAT10_HOME/bin/catalina.sh run when present.
// Placeholder probes: Selector.open (NIO connector).
// Today's baseline pins the WP0.1 println NPE failure mode
// (memory/finding_println_regression.md). When WP0.1 lands and the rc flips
// to 0, bench-baseline.json should be updated in the same PR.
public class Main {
    public static void main(String[] args) throws Exception {
        System.out.println("tomcat10_fixture: starting baseline smoke");
        System.out.println("tomcat10_fixture: Selector.open (NIO connector) probe");
        java.nio.channels.Selector sel = java.nio.channels.Selector.open();
        sel.close();
        System.out.println("tomcat10_fixture: ok");
    }
}
