// bench/gradle/fixture/Main.java
// WP8.7 placeholder fixture for Gradle 8 daemon forcing-function smoke.
//
// Real Gradle smoke runs `gradle --version` from $GRADLE_HOME.
// Placeholder probes: MD5 digest (Gradle cache keys).
// Today's baseline pins the WP0.1 println NPE failure mode
// (memory/finding_println_regression.md). When WP0.1 lands and the rc flips
// to 0, bench-baseline.json should be updated in the same PR.
public class Main {
    public static void main(String[] args) throws Exception {
        System.out.println("gradle_fixture: starting baseline smoke");
        System.out.println("gradle_fixture: MD5 digest (Gradle cache keys) probe");
        java.security.MessageDigest md = java.security.MessageDigest.getInstance("MD5");
        byte[] out = md.digest(new byte[]{1, 2, 3});
        if (out.length != 16) throw new AssertionError("md5 length wrong");
        System.out.println("gradle_fixture: ok");
    }
}
