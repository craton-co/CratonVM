// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 98, lane L8 -- `ObjectInputStream.resolveClass`
// resolves a stream class through `VM.latestUserDefinedLoader()`, HotSpot's
// `JVM_LatestUserDefinedLoader`: the loader of the NEAREST frame whose class
// was defined by a loader other than the boot and platform loaders,
// reflection frames (boot-loaded) skipped. CratonVM answers it from
// `classloader.rs` `latest_user_defined_loader_class`.
//
// A child-first loader defines its own copies of `Payload` and `Reader`. The
// same stream is read from several call shapes; each row names the loader of
// the class the deserialized object got.
//
//   main          read by main (application loader)      -> app
//   child         Reader.read, called directly through Method.invoke -> child
//   child-nested  Reader.readReflectively: the child class reflects on itself -> child
//   child-mh      a MethodHandle to Reader.read                     -> child
//   callback      Reader.readVia(f): the child frame calls back into a lambda
//                 of main that reads; the nearest user frame is main's -> app
//
// Run (stdout must be the block below, byte for byte; the class files must be
// on the class path as compiled, the loader reads them as resources):
//   $J/javac -d out tools/probes/interp/L8/L8W98LatestUserLoader.java
//   $B --java-home $J/.. -cp out L8W98LatestUserLoader
//   $B --compatible --java-home $J/.. -cp out L8W98LatestUserLoader
//   (each also with --nojit)
//   $J/java -cp out L8W98LatestUserLoader
//
// HOTSPOT_EXPECTED_BEGIN (derived from the JDK 25 / HotSpot sources, not measured)
// written-by child
// main app v=3
// child child v=3
// child-nested child v=3
// child-mh child v=3
// callback app v=3
// HOTSPOT_EXPECTED_END
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.ObjectInputStream;
import java.io.ObjectOutputStream;
import java.io.Serializable;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.function.Function;

public class L8W98LatestUserLoader {
    public static class Payload implements Serializable {
        private static final long serialVersionUID = 1L;
        public int v = 3;
    }

    public static class Reader {
        public static Object read(byte[] b) throws Exception {
            try (ObjectInputStream in = new ObjectInputStream(new ByteArrayInputStream(b))) {
                return in.readObject();
            }
        }

        public static Object readReflectively(byte[] b) throws Exception {
            return Reader.class.getMethod("read", byte[].class).invoke(null, (Object) b);
        }

        public static Object readVia(Function<byte[], Object> f, byte[] b) {
            return f.apply(b);
        }
    }

    static final class ChildFirst extends ClassLoader {
        ChildFirst(ClassLoader parent) {
            super("childfirst", parent);
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (name.equals("L8W98LatestUserLoader$Payload") || name.equals("L8W98LatestUserLoader$Reader")) {
                synchronized (getClassLoadingLock(name)) {
                    Class<?> c = findLoadedClass(name);
                    if (c == null) {
                        try (InputStream in = getParent().getResourceAsStream(name + ".class")) {
                            byte[] bytes = in.readAllBytes();
                            c = defineClass(name, bytes, 0, bytes.length);
                        } catch (IOException e) {
                            throw new ClassNotFoundException(name, e);
                        }
                    }
                    return c;
                }
            }
            return super.loadClass(name, resolve);
        }
    }

    static ClassLoader child;
    static ClassLoader app;

    static Object readHere(byte[] b) {
        try (ObjectInputStream in = new ObjectInputStream(new ByteArrayInputStream(b))) {
            return in.readObject();
        } catch (Exception e) {
            return e;
        }
    }

    static String who(Object o) throws Exception {
        if (o instanceof Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
        ClassLoader l = o.getClass().getClassLoader();
        String name = l == child ? "child" : l == app ? "app" : String.valueOf(l);
        return name + " v=" + o.getClass().getField("v").getInt(o);
    }

    public static void main(String[] args) throws Exception {
        app = L8W98LatestUserLoader.class.getClassLoader();
        child = new ChildFirst(app);
        Class<?> payloadC = child.loadClass("L8W98LatestUserLoader$Payload");
        Object payload = payloadC.getDeclaredConstructor().newInstance();
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        try (ObjectOutputStream out = new ObjectOutputStream(bytes)) {
            out.writeObject(payload);
        }
        byte[] b = bytes.toByteArray();
        System.out.println("written-by " + (payload.getClass().getClassLoader() == child ? "child" : "app"));

        System.out.println("main " + who(readHere(b)));

        Class<?> readerC = child.loadClass("L8W98LatestUserLoader$Reader");
        System.out.println("child " + who(readerC.getMethod("read", byte[].class).invoke(null, (Object) b)));
        System.out.println("child-nested "
                + who(readerC.getMethod("readReflectively", byte[].class).invoke(null, (Object) b)));
        MethodHandle mh = MethodHandles.publicLookup().findStatic(readerC, "read",
                MethodType.methodType(Object.class, byte[].class));
        Object viaMh;
        try {
            viaMh = mh.invoke(b);
        } catch (Throwable t) {
            viaMh = t;
        }
        System.out.println("child-mh " + who(viaMh));
        Function<byte[], Object> f = L8W98LatestUserLoader::readHere;
        System.out.println("callback " + who(readerC.getMethod("readVia", Function.class, byte[].class)
                .invoke(null, f, b)));
    }
}
