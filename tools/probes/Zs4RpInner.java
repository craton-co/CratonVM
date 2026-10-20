// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zs s4/rp (2026-10-15): per-class diagnostic for the Zs2Fp2Meta `classes`
// checksum gap (docs/internal/zgc-round-zs-20261014/s4-rp-report.md).
// Zs2Fp2Meta hashes forName + getDeclaredClasses (simple names, modifiers) +
// getEnclosingClass + getNestHost into one number; this probe prints the same
// answers (and a few neighbours) one line per class, so a byte diff against
// HotSpot names the class and the field.
//
// Usage:
//   Zs4RpInner classes <n> <file>   the first n names of <file> (Zr7RsBoot list,
//                                   made on HotSpot), one line each:
//     <name> | decl=<simple>:<mods>,... | enc=<name> | dcl=<name> | nest=<name>
//            | simple=<s> | flags=<anon><local><member> | mods=<int>
//   A field that throws prints `!<ExceptionClass>` in its place; a name that
//   forName refuses prints `<name> | forName!<ExceptionClass>`.
// Every stdout line is deterministic and must equal HotSpot's byte for byte.
import java.io.BufferedReader;
import java.io.FileReader;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Comparator;
import java.util.List;
import java.util.function.Supplier;

public class Zs4RpInner {
    static String safe(Supplier<String> s) {
        try {
            return s.get();
        } catch (Throwable t) {
            return "!" + t.getClass().getName();
        }
    }

    static String name(Class<?> c) {
        return c == null ? "-" : c.getName();
    }

    static String line(Class<?> c) {
        StringBuilder b = new StringBuilder(c.getName());
        b.append(" | decl=").append(safe(() -> {
            Class<?>[] decl = c.getDeclaredClasses();
            Arrays.sort(decl, Comparator.comparing(Class::getName));
            StringBuilder d = new StringBuilder();
            for (Class<?> x : decl) {
                if (d.length() > 0) {
                    d.append(',');
                }
                d.append(safe(x::getSimpleName)).append(':').append(x.getModifiers());
            }
            return d.toString();
        }));
        b.append(" | enc=").append(safe(() -> name(c.getEnclosingClass())));
        b.append(" | dcl=").append(safe(() -> name(c.getDeclaringClass())));
        b.append(" | nest=").append(safe(() -> name(c.getNestHost())));
        b.append(" | simple=").append(safe(c::getSimpleName));
        b.append(" | flags=").append(safe(() -> (c.isAnonymousClass() ? "A" : "-")
                + (c.isLocalClass() ? "L" : "-") + (c.isMemberClass() ? "M" : "-")));
        b.append(" | mods=").append(c.getModifiers());
        return b.toString();
    }

    public static void main(String[] args) throws Exception {
        int n = Integer.parseInt(args[1]);
        List<String> names = new ArrayList<>();
        try (BufferedReader r = new BufferedReader(new FileReader(args[2]))) {
            String l;
            while ((l = r.readLine()) != null && names.size() < n) {
                l = l.trim();
                if (!l.isEmpty()) {
                    names.add(l);
                }
            }
        }
        for (String nm : names) {
            Class<?> c;
            try {
                c = Class.forName(nm, false, null);
            } catch (Throwable t) {
                System.out.println(nm + " | forName!" + t.getClass().getName());
                continue;
            }
            System.out.println(line(c));
        }
        System.out.println("zs4rp mode=classes n=" + n + " done");
    }
}
