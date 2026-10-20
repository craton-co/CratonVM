// Warm the current H2 CompressLZF under JIT, then run H2 1.2.120's through a second loader (OSR by-name bind -> AIOOBE/SIGSEGV). Classpath: <h2 target/classes>.
//
// Record: nonpassed-classbyclass-census-RESOLVED-20260923.md (D5)
// Run: cratonvm --java-home <jdk-25> -c <dir>[:<classpath>] LzfTwo <h2-1.2.120.jar> cold|warm
// Compare with the same command on HotSpot (java -cp ...).
import java.lang.reflect.*;
import java.net.*;

public class LzfTwo {
    public static void main(String[] a) throws Exception {
        boolean warm = a[1].equals("warm");
        if (warm) {
            byte[] in = new byte[2048]; byte[] out = new byte[5000];
            org.h2.compress.CompressLZF lzf = new org.h2.compress.CompressLZF();
            for (int i = 0; i < 30000; i++) { in[i % 2048] = (byte) i; lzf.compress(in, 0, in.length, out, 0); }
            System.out.println("warmed new CompressLZF");
        }
        URLClassLoader cl = new URLClassLoader(new URL[]{new java.io.File(a[0]).toURI().toURL()}, ClassLoader.getPlatformClassLoader());
        Class<?> c = cl.loadClass("org.h2.compress.CompressLZF");
        Object lzf = c.getConstructor().newInstance();
        Method m = c.getMethod("compress", byte[].class, int.class, byte[].class, int.class);
        int ok = 0, fail = 0; Throwable first = null;
        for (int i = 0; i < 3000; i++) {
            byte[] in = new byte[2048]; in[i % 2048] = (byte) i; in[7] = 3;
            byte[] out = new byte[5000];
            try { m.invoke(lzf, in, in.length, out, 0); ok++; }
            catch (InvocationTargetException e) { fail++; if (first == null) first = e.getCause(); }
        }
        System.out.println("old: ok=" + ok + " fail=" + fail);
        if (first != null) { System.out.println(first); for (StackTraceElement e : first.getStackTrace()) { System.out.println("   " + e); if (e.getClassName().startsWith("java")) break; } }
    }
}
