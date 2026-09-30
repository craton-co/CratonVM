// In-process javac compiles (ToolProvider.getSystemJavaCompiler), the H2 SourceCompiler path.
//
// Record: fs-cluster-needtoresolveagainstdefaultdirectory-FIXED-20260923.md
// Run: cratonvm --java-home <jdk-25> -c <dir>[:<classpath>] JavacProbe [iterations]
// Compare with the same command on HotSpot (java -cp ...).
import javax.tools.*;
import java.io.*;
import java.net.URI;
import java.util.*;

public class JavacProbe {
    static class Src extends SimpleJavaFileObject {
        final String code;
        Src(String name, String code) { super(URI.create("string:///" + name + ".java"), Kind.SOURCE); this.code = code; }
        public CharSequence getCharContent(boolean ignore) { return code; }
    }
    public static void main(String[] a) throws Exception {
        int iters = a.length > 0 ? Integer.parseInt(a[0]) : 5;
        JavaCompiler jc = ToolProvider.getSystemJavaCompiler();
        System.out.println("compiler=" + jc);
        int ok = 0;
        for (int i = 0; i < iters; i++) {
            long t0 = System.nanoTime();
            StringWriter out = new StringWriter();
            StandardJavaFileManager fm = jc.getStandardFileManager(null, null, null);
            File dir = new File(System.getProperty("java.io.tmpdir"), "jcp-" + ProcessHandle.current().pid() + "-" + i);
            dir.mkdirs();
            fm.setLocation(StandardLocation.CLASS_OUTPUT, List.of(dir));
            String code = "public class Rev" + i + " { public static String reverse(String s) { return new StringBuilder(s).reverse().toString(); } }";
            try {
                Boolean r = jc.getTask(out, fm, null, List.of("-proc:none"), null, List.of(new Src("Rev" + i, code))).call();
                if (Boolean.TRUE.equals(r)) ok++;
                System.out.println("iter " + i + " result=" + r + " ms=" + (System.nanoTime() - t0) / 1_000_000 + " out=" + out.toString().trim());
            } catch (Throwable t) {
                System.out.println("iter " + i + " THREW " + t);
                Throwable c = t; while (c.getCause() != null) c = c.getCause();
                c.printStackTrace(System.out);
            }
            fm.close();
        }
        System.out.println("ok=" + ok + "/" + iters);
    }
}
