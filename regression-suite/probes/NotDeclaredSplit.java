import java.io.*;
import java.lang.reflect.*;
import java.nio.file.*;
import java.util.*;

/**
 * Split "the real class does not DECLARE this method" into the three different
 * things it actually means.
 *
 * G79-1 §2 found the jdk-only audit's absent-method counts inflate genuine
 * absence roughly fourfold, because "not declared here" silently merges:
 *
 *   INHERITED-CONCRETE  the method exists with a body, on a superclass
 *   ABSTRACT-INTERFACE  the method is abstract, declared on an interface
 *                       -- the dangerous kind: a native registered here
 *                       decides dispatch for every USER implementor
 *   ABSENT              nothing of that name exists at all
 *
 * The distinction is not academic for the P0 over-tagging row.
 * `native-collections`' own file header warns that its 214 abstract-interface
 * registrations "decide dispatch for every USER subclass, not just for
 * `java.util` classes" — so a registrar's safety to retag turns on how many of
 * its not-declared-here rows are of that kind, and the raw count cannot say.
 *
 * Input: lines of "class name descriptor registrar" (internal class form).
 * Output: one verdict per line, plus a per-registrar summary.
 */
public class NotDeclaredSplit {
    public static void main(String[] a) throws Exception {
        Map<String, int[]> byReg = new TreeMap<>();   // [inherited, abstractIface, absent, total]
        List<String> ifaceRows = new ArrayList<>();
        ClassLoader cl = NotDeclaredSplit.class.getClassLoader();

        for (String line : Files.readAllLines(Paths.get(a[0]))) {
            String[] p = line.trim().split(" ");
            if (p.length < 4) continue;
            String cn = p[0].replace('/', '.'), mn = p[1], reg = p[3];
            int[] c = byReg.computeIfAbsent(reg, k -> new int[4]);
            c[3]++;
            try {
                Class<?> k = Class.forName(cn, false, cl);
                // `<init>` is NOT a Method: getMethods() never returns
                // constructors, so asking it about one reports a false ABSENT.
                // Measured the hard way — `CopyOnWriteArraySet.<init>` came back
                // absent from a class that plainly has constructors.
                if (mn.equals("<init>")) {
                    if (k.getDeclaredConstructors().length > 0) { c[0]++; } else { c[2]++; }
                    continue;
                }
                Method found = null;
                for (Method m : k.getMethods()) {
                    if (m.getName().equals(mn)) { found = m; break; }
                }
                if (found == null) {
                    c[2]++;
                } else if (Modifier.isAbstract(found.getModifiers())
                        && found.getDeclaringClass().isInterface()) {
                    c[1]++;
                    ifaceRows.add(reg + "  " + cn + "." + mn
                            + "  (declared on " + found.getDeclaringClass().getName() + ")");
                } else {
                    c[0]++;
                }
            } catch (Throwable t) {
                c[2]++;
            }
        }

        System.out.printf("%-46s %5s %5s %5s %5s%n",
                "registrar", "total", "inher", "IFACE", "absent");
        for (Map.Entry<String, int[]> e : byReg.entrySet()) {
            int[] c = e.getValue();
            System.out.printf("%-46s %5d %5d %5d %5d%n", e.getKey(), c[3], c[0], c[1], c[2]);
        }
        int ti = 0, tf = 0, tb = 0;
        for (int[] c : byReg.values()) { ti += c[0]; tf += c[1]; tb += c[2]; }
        System.out.println();
        System.out.println("TOTAL inherited-concrete = " + ti
                + "   ABSTRACT-INTERFACE = " + tf + "   absent = " + tb);
    }
}
