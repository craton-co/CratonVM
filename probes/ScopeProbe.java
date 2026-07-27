import java.util.*;

/** Narrow the javac "duplicate element 'value'" bug: identity-hash sets vs javac Scope. */
public class ScopeProbe {

    static class Plain { }          // no equals/hashCode override -> identity

    public static void main(String[] args) throws Exception {
        // 1. identity-keyed LinkedHashSet add/remove round-trip
        Set<Plain> s = new LinkedHashSet<>();
        List<Plain> made = new ArrayList<>();
        for (int i = 0; i < 5; i++) { Plain p = new Plain(); made.add(p); s.add(p); }
        int removed = 0;
        for (Plain p : made) if (s.remove(p)) removed++;
        System.out.println("1 LinkedHashSet identity remove: " + removed + "/5  leftover=" + s.size());

        // 2. hashCode stability
        Plain p = new Plain();
        int h1 = p.hashCode(), h2 = p.hashCode(), h3 = System.identityHashCode(p);
        System.out.println("2 hashCode stable: " + (h1 == h2) + " hashCode==identityHashCode: "
                + (h1 == h3) + "  (" + h1 + "," + h2 + "," + h3 + ")");

        // 3. javac Scope.getSymbols over an annotation type's members
        Class<?> ctxC = Class.forName("com.sun.tools.javac.util.Context");
        Object ctx = ctxC.getDeclaredConstructor().newInstance();
        Class<?> javacFileMgr = Class.forName("com.sun.tools.javac.file.JavacFileManager");
        javacFileMgr.getMethod("preRegister", ctxC).invoke(null, ctx);
        Class<?> symtabC = Class.forName("com.sun.tools.javac.code.Symtab");
        Object symtab = symtabC.getMethod("instance", ctxC).invoke(null, ctx);
        Class<?> namesC = Class.forName("com.sun.tools.javac.util.Names");
        Object names = namesC.getMethod("instance", ctxC).invoke(null, ctx);
        Class<?> classFinderC = Class.forName("com.sun.tools.javac.code.ClassFinder");
        Object finder = classFinderC.getMethod("instance", ctxC).invoke(null, ctx);

        Object fromString = namesC.getMethod("fromString", String.class)
                .invoke(names, "java.lang.SuppressWarnings");
        Object modSym = symtabC.getField("java_base").get(symtab);
        java.lang.reflect.Method loadClass = classFinderC.getMethod("loadClass",
                Class.forName("com.sun.tools.javac.code.Symbol$ModuleSymbol"),
                Class.forName("com.sun.tools.javac.util.Name"));
        Object clsSym = loadClass.invoke(finder, modSym, fromString);
        System.out.println("3 loaded ClassSymbol: " + clsSym);

        Object scope = clsSym.getClass().getMethod("members").invoke(clsSym);
        Class<?> lookupKind = Class.forName("com.sun.tools.javac.code.Scope$LookupKind");
        Object nonRecursive = null;
        for (Object o : lookupKind.getEnumConstants())
            if (o.toString().equals("NON_RECURSIVE")) nonRecursive = o;
        Iterable<?> syms = (Iterable<?>) scope.getClass()
                .getMethod("getSymbols", lookupKind).invoke(scope, nonRecursive);
        int n = 0;
        for (Object sym : syms) { n++; System.out.println("   sym: " + sym + " kind=" + sym.getClass().getSimpleName()); }
        System.out.println("3 total symbols in SuppressWarnings scope: " + n);
    }
}
