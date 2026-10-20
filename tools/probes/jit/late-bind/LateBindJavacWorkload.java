import javax.tools.*;
import java.nio.file.*;
import java.security.MessageDigest;
import java.util.*;
import java.util.stream.*;

// Big-workload miscompile detector: compile a JDK source set in-process N times,
// digest the class files, then check StringBuilder.append(double) like a
// start-up logger. Prints "DIGEST <hex>" per round and one verdict line.
public class LateBindJavacWorkload {
    public static void main(String[] a) throws Exception {
        Path src = Paths.get(a[0]);           // e.g. .../src/java.base
        String pkg = a[1];                    // e.g. java/util
        int rounds = Integer.parseInt(a[2]);
        String expect = a.length > 3 ? a[3] : null;
        List<String> files;
        try (Stream<Path> s = Files.list(src.resolve(pkg))) {
            files = s.filter(p -> p.toString().endsWith(".java")).map(Path::toString).sorted().collect(Collectors.toList());
        }
        JavaCompiler jc = ToolProvider.getSystemJavaCompiler();
        String verdict = "PASS";
        Throwable first = null;
        long bad = 0;
        java.util.Random r = new java.util.Random(11);
        for (int round = 0; round < rounds; round++) {
            Path out = Files.createTempDirectory("latebind");
            List<String> argv = new ArrayList<>(List.of("-nowarn", "-XDignore.symbol.file", "-proc:none",
                "--patch-module", "java.base=" + src, "-d", out.toString(), "-Xlint:none", "-XDsuppressNotes"));
            argv.addAll(files);
            java.io.StringWriter err = new java.io.StringWriter();
            java.io.ByteArrayOutputStream diag = new java.io.ByteArrayOutputStream();
            int rc = jc.run(null, null, new java.io.PrintStream(diag, true), argv.toArray(new String[0]));
            if (rc != 0) {
                String[] lines = diag.toString().split("\n");
                for (int i = 0; i < Math.min(60, lines.length); i++) System.out.println("JAVAC| " + lines[i]);
            }
            MessageDigest md = MessageDigest.getInstance("SHA-256");
            List<Path> outs;
            try (Stream<Path> s = Files.walk(out)) { outs = s.filter(Files::isRegularFile).sorted().collect(Collectors.toList()); }
            for (Path p : outs) { md.update(out.relativize(p).toString().getBytes()); md.update(Files.readAllBytes(p)); }
            String hex = HexFormat.of().formatHex(md.digest());
            System.out.println("DIGEST rc=" + rc + " n=" + outs.size() + " " + hex);
            if (expect != null && !expect.equals(hex)) verdict = "FAIL digest";
            for (Path p : outs) Files.delete(p);
            // start-up-logger shaped formatting
            for (int i = 0; i < 3000; i++) {
                double d = (r.nextInt(20000) + 1) / 1000.0;
                try {
                    String s = new StringBuilder("Started in ").append(d).append(" seconds").toString();
                    if (Double.parseDouble(s.substring(11, s.length() - 8)) != d) bad++;
                } catch (Throwable t) { bad++; if (first == null) first = t; }
            }
        }
        if (first != null) {
            System.out.println("FAIL append first=" + first);
            for (StackTraceElement e : first.getStackTrace()) { System.out.println("  at " + e); }
        } else if (bad != 0) System.out.println("FAIL append bad=" + bad);
        else System.out.println(verdict);
    }
}
