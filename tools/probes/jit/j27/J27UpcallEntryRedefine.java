// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Agent manifest: Premain-Class: J27UpcallEntryRedefine$Agent
//                 Can-Retransform-Classes: true
import java.lang.foreign.*;
import java.lang.instrument.*;
import java.lang.invoke.*;
import java.nio.charset.StandardCharsets;
import java.security.ProtectionDomain;
import static java.lang.foreign.ValueLayout.*;

public class J27UpcallEntryRedefine {
    static Instrumentation instrumentation;
    public static class Agent {
        public static void premain(String args, Instrumentation inst) { instrumentation = inst; }
    }
    public static class Target {
        static String source;
        static String walk;
        public static int entry(int x) {
            if (x < 0) {
                try { throw new IllegalStateException("source"); }
                catch (IllegalStateException e) { source = e.getStackTrace()[0].getFileName(); }
                walk = StackWalker.getInstance().walk(frames -> frames.findFirst().orElseThrow().getFileName());
            }
            return x + 1;
        }
    }
    static class Rewrite implements ClassFileTransformer {
        final boolean removeSource;
        Rewrite(boolean removeSource) { this.removeSource = removeSource; }
        public byte[] transform(ClassLoader loader, String name, Class<?> type,
                ProtectionDomain domain, byte[] bytes) {
            if (!name.equals("J27UpcallEntryRedefine$Target")) return null;
            byte[] out = bytes.clone();
            boolean changed = false;
            for (int i = 0; i + 3 < out.length; i++) {
                if (out[i] == 0x1a && out[i+1] == 4 && out[i+2] == 0x60 && out[i+3] == (byte)0xac) {
                    out[i+1] = 5;
                    changed = true;
                }
            }
            if (!changed) throw new AssertionError("target pattern missing");
            byte[] old = "J27UpcallEntryRedefine.java".getBytes(StandardCharsets.UTF_8);
            byte[] fresh = "J27UpcallEntryReplaced.java".getBytes(StandardCharsets.UTF_8);
            for (int i = 0; i + old.length <= out.length; i++) {
                boolean same = true;
                for (int k = 0; k < old.length; k++) same &= out[i+k] == old[k];
                if (same) System.arraycopy(fresh, 0, out, i, fresh.length);
            }
            return removeSource ? withoutSourceFile(out) : out;
        }
    }
    static int u2(byte[] b, int at) { return (b[at] & 255) * 256 + (b[at+1] & 255); }
    static int u4(byte[] b, int at) { return u2(b, at) * 65536 + u2(b, at+2); }
    static byte[] withoutSourceFile(byte[] b) {
        int p = 10, sourceIndex = 0;
        int count = u2(b, 8);
        for (int i = 1; i < count; i++) {
            int tag = b[p++] & 255;
            switch (tag) {
                case 1 -> {
                    int n = u2(b, p); p += 2;
                    if (new String(b, p, n, StandardCharsets.UTF_8).equals("SourceFile")) sourceIndex = i;
                    p += n;
                }
                case 3, 4, 9, 10, 11, 12, 17, 18 -> p += 4;
                case 5, 6 -> { p += 8; i++; }
                case 7, 8, 16, 19, 20 -> p += 2;
                case 15 -> p += 3;
                default -> throw new AssertionError("constant tag " + tag);
            }
        }
        p += 6;
        int interfaces = u2(b, p); p += 2 + interfaces * 2;
        for (int members = 0; members < 2; members++) {
            int n = u2(b, p); p += 2;
            for (int i = 0; i < n; i++) {
                p += 6;
                int attributes = u2(b, p); p += 2;
                for (int a = 0; a < attributes; a++) p += 6 + u4(b, p+2);
            }
        }
        int countAt = p;
        int attributes = u2(b, p); p += 2;
        for (int a = 0; a < attributes; a++) {
            int end = p + 6 + u4(b, p+2);
            if (u2(b, p) == sourceIndex) {
                byte[] out = new byte[b.length - (end-p)];
                System.arraycopy(b, 0, out, 0, p);
                System.arraycopy(b, end, out, p, b.length-end);
                out[countAt] = (byte)((attributes-1) >>> 8);
                out[countAt+1] = (byte)(attributes-1);
                return out;
            }
            p = end;
        }
        throw new AssertionError("SourceFile attribute missing");
    }
    public static void main(String[] args) throws Throwable {
        if (instrumentation == null) throw new AssertionError("run with -javaagent");
        Linker linker = Linker.nativeLinker();
        FunctionDescriptor fd = FunctionDescriptor.of(JAVA_INT, JAVA_INT);
        MethodHandle target = MethodHandles.lookup().findStatic(Target.class, "entry",
            MethodType.methodType(int.class, int.class));
        try (Arena arena = Arena.ofConfined()) {
            MethodHandle call = linker.downcallHandle(linker.upcallStub(target, fd, arena), fd);
            int result = 0;
            for (int i = 0; i < 8000; i++) result = (int) call.invokeExact(1000);
            System.out.println("fact before " + result);
            boolean removeSource = args.length > 0 && args[0].equals("remove-source");
            if (args.length == 0 || removeSource) result = (int) call.invokeExact(-1);
            System.out.println("fact source-before " + Target.source);
            System.out.println("fact walk-before " + Target.walk);
            Rewrite rewrite = new Rewrite(removeSource);
            instrumentation.addTransformer(rewrite, true);
            instrumentation.retransformClasses(Target.class);
            System.out.println("fact after " + (int) call.invokeExact(1000));
            result = (int) call.invokeExact(-1);
            System.out.println("fact source-after " + Target.source);
            System.out.println("fact walk-after " + Target.walk);
            for (int i = 0; i < 8000; i++) result = (int) call.invokeExact(1000);
            System.out.println("fact rewarmed " + result);
            if (removeSource) {
                instrumentation.removeTransformer(rewrite);
                instrumentation.addTransformer(new Rewrite(false), true);
                instrumentation.retransformClasses(Target.class);
                result = (int) call.invokeExact(-1);
                System.out.println("fact source-restored " + Target.source);
                System.out.println("fact walk-restored " + Target.walk);
                System.out.println("fact result-restored " + (int) call.invokeExact(1000));
            }
        }
    }
}

class J27UpcallEntryRedefineCold {
    public static void main(String[] args) throws Throwable {
        J27UpcallEntryRedefine.main(new String[] { "cold-source" });
    }
}

class J27UpcallEntryRedefineRemoveSource {
    public static void main(String[] args) throws Throwable {
        J27UpcallEntryRedefine.main(new String[] { "remove-source" });
    }
}
