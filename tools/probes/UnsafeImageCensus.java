import java.io.BufferedReader;
import java.io.FileReader;
import java.lang.reflect.Method;
import java.lang.reflect.Modifier;
import java.util.ArrayList;
import java.util.List;

/** Does the JDK image running this program DECLARE the method each CratonVM
 *  `Unsafe` registration stands in front of, and is it native or bytecode?
 *
 *  This is the multi-image half of the retirement question. A registration that
 *  shadows nothing on JDK 25 may be the only implementation on JDK 17 -- and
 *  `WORKER-3-NOTE-2-the-multi-image-method-sweep-says-192-not-342` is the record
 *  of what happens when one image's evidence is taken for the answer.
 *
 *  PURE REFLECTION, no compile-time reference to either Unsafe class and no
 *  `setAccessible`, so it needs no `--add-exports` and no `--add-opens` and runs
 *  on any image from one `--release 17` build. `getDeclaredMethod` is a lookup;
 *  it does not open anything.
 *
 *  Input: one `<class> <name> <descriptor>` triple per line, internal spelling.
 *  Output: `<class> <name> <descriptor> |<verdict>|`, verdict one of
 *  ABSENT / native / bytecode / not-a-method / NO-SUCH-CLASS / PARAM-TYPE-ABSENT.
 */
public class UnsafeImageCensus {

    /** JVM descriptor -> parameter Class[]. A parameter type that is itself
     *  absent from this image is a DIFFERENT answer from the method being
     *  absent, and is reported as such rather than folded into ABSENT. */
    static Class<?>[] params(String desc) throws ClassNotFoundException {
        List<Class<?>> out = new ArrayList<>();
        int i = desc.indexOf('(') + 1;
        int end = desc.indexOf(')');
        while (i < end) {
            int dims = 0;
            while (desc.charAt(i) == '[') { dims++; i++; }
            Class<?> base;
            char c = desc.charAt(i);
            switch (c) {
                case 'Z': base = boolean.class; i++; break;
                case 'B': base = byte.class;    i++; break;
                case 'C': base = char.class;    i++; break;
                case 'S': base = short.class;   i++; break;
                case 'I': base = int.class;     i++; break;
                case 'J': base = long.class;    i++; break;
                case 'F': base = float.class;   i++; break;
                case 'D': base = double.class;  i++; break;
                case 'L': {
                    int semi = desc.indexOf(';', i);
                    String n = desc.substring(i + 1, semi).replace('/', '.');
                    base = Class.forName(n, false, ClassLoader.getSystemClassLoader());
                    i = semi + 1;
                    break;
                }
                default: throw new IllegalArgumentException("bad descriptor " + desc);
            }
            for (int d = 0; d < dims; d++) {
                base = java.lang.reflect.Array.newInstance(base, 0).getClass();
            }
            out.add(base);
        }
        return out.toArray(new Class<?>[0]);
    }

    public static void main(String[] args) throws Exception {
        System.out.println("IMAGE " + System.getProperty("java.version")
                           + " (" + System.getProperty("java.vm.version") + ")");
        int absent = 0, nat = 0, code = 0, noclass = 0, badparam = 0, notmethod = 0;
        try (BufferedReader r = new BufferedReader(new FileReader(args[0]))) {
            String line;
            while ((line = r.readLine()) != null) {
                line = line.trim();
                if (line.isEmpty()) continue;
                String[] p = line.split("\\s+");
                if (p.length < 3) continue;
                String cls = p[0], name = p[1], desc = p[2];
                String verdict;
                try {
                    Class<?> c = Class.forName(cls.replace('/', '.'), false,
                                               ClassLoader.getSystemClassLoader());
                    try {
                        Method m = c.getDeclaredMethod(name, params(desc));
                        boolean isNative = Modifier.isNative(m.getModifiers());
                        verdict = isNative ? "native" : "bytecode";
                        if (isNative) nat++; else code++;
                    } catch (NoSuchMethodException e) {
                        // `<clinit>` and `<init>` are not reachable through
                        // getDeclaredMethod. Saying so is a different claim
                        // from saying the image does not declare them.
                        if (name.startsWith("<")) { verdict = "not-a-method"; notmethod++; }
                        else { verdict = "ABSENT"; absent++; }
                    } catch (ClassNotFoundException e) {
                        verdict = "PARAM-TYPE-ABSENT " + e.getMessage();
                        badparam++;
                    }
                } catch (ClassNotFoundException e) {
                    verdict = "NO-SUCH-CLASS";
                    noclass++;
                }
                System.out.println(cls + " " + name + " " + desc + " |" + verdict + "|");
            }
        }
        System.out.println("TOTALS absent=" + absent + " native=" + nat
                           + " bytecode=" + code + " no-such-class=" + noclass
                           + " param-type-absent=" + badparam
                           + " not-a-method=" + notmethod);
    }
}
