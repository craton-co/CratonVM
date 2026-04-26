// bench/maven/fixture/Main.java
// WP8.7 placeholder fixture for Maven 3.9 forcing-function smoke.
//
// Real Maven smoke runs `mvn -version` and `mvn dependency:resolve` against a tiny pom.xml.
// Placeholder probes: NIO Path (Maven startup).
// Today's baseline pins the WP0.1 println NPE failure mode
// (memory/finding_println_regression.md). When WP0.1 lands and the rc flips
// to 0, bench-baseline.json should be updated in the same PR.
public class Main {
    public static void main(String[] args) throws Exception {
        System.out.println("maven_fixture: starting baseline smoke");
        System.out.println("maven_fixture: NIO Path (Maven startup) probe");
        java.nio.file.Path p = java.nio.file.Paths.get(".").toAbsolutePath().normalize();
        if (p == null) throw new AssertionError("path null");
        System.out.println("maven_fixture: ok");
    }
}
