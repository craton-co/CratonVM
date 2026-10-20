// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 81, lane L4 -- page
// `docs/internal/fixed-bugs/i72-L4-a-bootstrap-callers-converting-invoke-resolves-its-site-through-the-system-loader-FIXED-20261011.md`
// ("Fixed (wave 81)").
//
// A converting `invoke` whose call-site descriptor names a class NO loader can
// find. HotSpot links the `invokehandle` site with the caller's loader in
// `NCDFError` mode (`SystemDictionary::find_method_handle_type` ->
// `resolve_or_fail`): `NoClassDefFoundError: <internal name>`, caused by the
// application loader's `ClassNotFoundException`. CratonVM resolved the site's
// `MethodType` with the JDK factory (`fromMethodDescriptorString`), whose
// `BytecodeDescriptor` raises `TypeNotPresentException: Type <name> not
// present`; wave 81 rewrites that failure to HotSpot's error
// (`lang_invoke.rs` `mh_site_type_link_error`, unconditional).
//
// The caller (`L4W81SiteTypeMissingCaller`) is generated here, byte by byte,
// because javac will not compile a reference to a class that does not exist;
// it is defined in this class's package and loader with
// `Lookup.defineClass`. Its one method is
//   public static Object call(MethodHandle mh) {
//       return mh.invoke((Object) "x");   // site (Object)LL4W81MissingType;
//   }
// No timing, no addresses: every line is deterministic.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// control: made:y
// caller-loader: app
// caller-method: call
// missing-site: java.lang.NoClassDefFoundError: L4W81MissingType
// missing-site-cause: java.lang.ClassNotFoundException: L4W81MissingType
// missing-site-again: java.lang.NoClassDefFoundError: L4W81MissingType
// HOTSPOT_EXPECTED_END
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public class L4W81SiteTypeMissingClass {
    static final String CALLER = "L4W81SiteTypeMissingCaller";
    static final String MISSING = "L4W81MissingType";

    public static Object make(Object o) {
        return "made:" + o;
    }

    static void utf8(DataOutputStream out, String s) throws IOException {
        out.writeByte(1);
        out.writeUTF(s);
    }

    /** The class file of `CALLER` (version 52: no branch, so no StackMapTable). */
    static byte[] callerBytes() throws IOException {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(bytes);
        out.writeInt(0xCAFEBABE);
        out.writeShort(0);
        out.writeShort(52);
        out.writeShort(16); // constant pool count (entries 1..15)
        utf8(out, CALLER); // 1
        out.writeByte(7); // 2 Class #1
        out.writeShort(1);
        utf8(out, "java/lang/Object"); // 3
        out.writeByte(7); // 4 Class #3
        out.writeShort(3);
        utf8(out, "java/lang/invoke/MethodHandle"); // 5
        out.writeByte(7); // 6 Class #5
        out.writeShort(5);
        utf8(out, "invoke"); // 7
        utf8(out, "(Ljava/lang/Object;)L" + MISSING + ";"); // 8
        out.writeByte(12); // 9 NameAndType #7:#8
        out.writeShort(7);
        out.writeShort(8);
        out.writeByte(10); // 10 Methodref #6.#9
        out.writeShort(6);
        out.writeShort(9);
        utf8(out, "x"); // 11
        out.writeByte(8); // 12 String #11
        out.writeShort(11);
        utf8(out, "call"); // 13
        utf8(out, "(Ljava/lang/invoke/MethodHandle;)Ljava/lang/Object;"); // 14
        utf8(out, "Code"); // 15
        out.writeShort(0x0001 | 0x0020); // ACC_PUBLIC | ACC_SUPER
        out.writeShort(2); // this_class
        out.writeShort(4); // super_class
        out.writeShort(0); // interfaces
        out.writeShort(0); // fields
        out.writeShort(1); // methods
        out.writeShort(0x0001 | 0x0008); // ACC_PUBLIC | ACC_STATIC
        out.writeShort(13);
        out.writeShort(14);
        out.writeShort(1); // method attributes
        out.writeShort(15); // Code
        // aload_0; ldc #12; invokevirtual #10; areturn
        byte[] code = {0x2a, 0x12, 12, (byte) 0xb6, 0, 10, (byte) 0xb0};
        out.writeInt(12 + code.length);
        out.writeShort(2); // max_stack
        out.writeShort(1); // max_locals
        out.writeInt(code.length);
        out.write(code);
        out.writeShort(0); // exception table
        out.writeShort(0); // code attributes
        out.writeShort(0); // class attributes
        out.flush();
        return bytes.toByteArray();
    }

    interface Row {
        Object run() throws Throwable;
    }

    static String text(Throwable t) {
        return t == null ? "null" : t.getClass().getName() + ": " + t.getMessage();
    }

    static void row(String name, Row r) {
        String out;
        try {
            out = String.valueOf(r.run());
        } catch (Throwable t) {
            out = text(t);
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) throws Throwable {
        // `(Object)Object`: every call below is a converting `invoke`.
        MethodHandle mh = MethodHandles.lookup().findStatic(L4W81SiteTypeMissingClass.class, "make",
                MethodType.methodType(Object.class, Object.class));
        row("control", () -> (String) mh.invoke((Object) "y"));

        Class<?> caller = MethodHandles.lookup().defineClass(callerBytes());
        row("caller-loader", () ->
                caller.getClassLoader() == ClassLoader.getSystemClassLoader() ? "app" : "other");
        // Inside a row (wave 81 rework): a failure here prints, instead of
        // ending the run with every later row missing.
        Method[] callRef = new Method[1];
        row("caller-method", () -> {
            callRef[0] = caller.getMethod("call", MethodHandle.class);
            return callRef[0].getName();
        });
        Method call = callRef[0];
        if (call == null) {
            return;
        }
        Throwable[] first = new Throwable[1];
        row("missing-site", () -> {
            try {
                return call.invoke(null, mh);
            } catch (InvocationTargetException e) {
                first[0] = e.getCause();
                throw e.getCause();
            }
        });
        System.out.println("missing-site-cause: " + (first[0] == null ? "none" : text(first[0].getCause())));
        row("missing-site-again", () -> {
            try {
                return call.invoke(null, mh);
            } catch (InvocationTargetException e) {
                throw e.getCause();
            }
        });
    }
}
