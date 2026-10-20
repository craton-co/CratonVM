// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 133, lane L5 -- the w131 fix-back of
// `L5W125GlobalNameRefusalRecord` (`CRATONVM_GLOBAL_NAME_INITIATING_RECORD`,
// default on, `=0` off). A loader that overrides only `findClass` (so it is
// "transparent" for runtime-image names: CratonVM's global route answers them
// without asking it) and whose parent is the platform loader defines two
// classes. HotSpot asks the loader for every name its classes resolve; its
// parent chain answers `javax.net.SocketFactory` and `java.util.zip.Adler32`,
// and the loader becomes an initiating loader of both (a dictionary row), so
// `findLoadedClass` answers them afterwards. `Class.forName(name, false, l)`
// makes the same row. Before wave 133 CratonVM recorded the `java/` name only.
//
// Run: javac -d . L5W133TransparentGlobalNameRecord.java
//      java -cp . L5W133TransparentGlobalNameRecord   (HotSpot)
//      cratonvm --jdk-only|--compatible -cp . L5W133TransparentGlobalNameRecord
//      CRATONVM_GLOBAL_NAME_INITIATING_RECORD=0 cratonvm --jdk-only -cp . L5W133TransparentGlobalNameRecord  (old answer: javax rows null)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags and -Xint)
// before javax=null java=null forname=null
// resolve javax=true java=true
// after javax=same java=same
// forname same=true find=same
// finds=0
// HOTSPOT_EXPECTED_END

import java.io.InputStream;
import java.lang.reflect.Method;

public class L5W133TransparentGlobalNameRecord {
    static final String JAVAX = "javax.net.SocketFactory";
    static final String JAVA = "java.util.zip.Adler32";
    static final String FORNAME = "javax.net.ServerSocketFactory";

    public static class Ref1 {
        public static Object get() {
            return new Object[] {javax.net.SocketFactory.class, java.util.zip.Adler32.class};
        }
    }

    static final class FindOnlyLoader extends ClassLoader {
        final ClassLoader source;
        int globalFinds;

        FindOnlyLoader(ClassLoader parent, ClassLoader source) {
            super(parent);
            this.source = source;
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            if (!name.startsWith("L5W133TransparentGlobalNameRecord$")) {
                globalFinds++;
                throw new ClassNotFoundException(name);
            }
            String res = name.replace('.', '/') + ".class";
            try (InputStream in = source.getResourceAsStream(res)) {
                byte[] b = in.readAllBytes();
                return defineClass(name, b, 0, b.length);
            } catch (java.io.IOException e) {
                throw new ClassNotFoundException(name, e);
            }
        }

        Class<?> peek(String name) {
            return findLoadedClass(name);
        }
    }

    static String find(FindOnlyLoader l, String name, Class<?> expected) {
        Class<?> c = l.peek(name);
        return c == null ? "null" : (c == expected ? "same" : "other");
    }

    public static void main(String[] args) throws Exception {
        Class<?> javax = javax.net.SocketFactory.class;
        Class<?> java = java.util.zip.Adler32.class;
        Class<?> forname = javax.net.ServerSocketFactory.class;
        ClassLoader app = L5W133TransparentGlobalNameRecord.class.getClassLoader();
        FindOnlyLoader l = new FindOnlyLoader(ClassLoader.getPlatformClassLoader(), app);
        System.out.println("before javax=" + find(l, JAVAX, javax) + " java=" + find(l, JAVA, java)
                + " forname=" + find(l, FORNAME, forname));
        Method m = l.loadClass("L5W133TransparentGlobalNameRecord$Ref1").getMethod("get");
        Object[] r = (Object[]) m.invoke(null);
        System.out.println("resolve javax=" + (r[0] == javax) + " java=" + (r[1] == java));
        System.out.println("after javax=" + find(l, JAVAX, javax) + " java=" + find(l, JAVA, java));
        Class<?> f = Class.forName(FORNAME, false, l);
        System.out.println("forname same=" + (f == forname) + " find=" + find(l, FORNAME, forname));
        System.out.println("finds=" + l.globalFinds);
    }
}
