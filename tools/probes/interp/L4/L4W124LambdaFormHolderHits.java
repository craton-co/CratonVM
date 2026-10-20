// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// probe (census)
//
// Interpreter round i6 wave 124, lane L4
// (docs/internal/proposals/interpreter/i88-L3-proposal-first-use-reflective-constructor-cost-20261121.md):
// does `getConstructor().newInstance()` of a fresh class compile (spin) any LambdaForm, or does
// every form on the path come from a pre-generated holder of java.base
// (`DirectMethodHandle$Holder`, `Invokers$Holder`, `DelegatingMethodHandle$Holder`,
// `LambdaForm$Holder`), as `InvokerBytecodeGenerator.lookupPregenerated` finds them on HotSpot?
//
// The fresh classes are assembled by hand (a 10-entry constant pool), NOT with java.lang.classfile:
// the wave-122 census driver built its classes with the ClassFile API, so its own class building
// (`AbstractPoolEntry`, `BufWriterImpl`) shows in the same interpreted-frame census as any
// LambdaForm spin would.
//
// What it reads, by reflection (`--add-opens`), after a warm-up:
//   - the accessor `newInstance` installed (`Constructor.getConstructorAccessor()`; the field is filtered) and its `target`;
//   - the target's `form` (`MethodHandle.form`), the form's `kind`, `customized` and `vmentry`, and
//     where `vmentry` lives: `holder` (a `...$Holder` class), `spun` (a hidden class: the
//     generator ran), `fabricated` (`LambdaForm` itself: CratonVM's C33 stand-in) or `null`;
//   - over 40 fresh classes: how many distinct target forms and entries there are (HotSpot: one
//     shared `LF_NEWINVSPECIAL` form per basic type, so 1 and 1);
//   - the `()Object` basic type's form cache (`MethodTypeForm.lambdaForms`) before and after the
//     measured phase: forms that appeared or changed during it, and present forms not from a
//     holder;
//   - the same for `MethodHandles.lookup().unreflectConstructor(k)` and its first `invoke()`.
// Stdout carries only counts and the shared target form (compared). The per-index listing of the
// form cache and every captured `[LF_RESOLVE]` line go to stderr (diagnostic, VM-specific).
//
// Optional census run: `-Djava.lang.invoke.MethodHandle.TRACE_RESOLVE=true` makes the JDK print an
// `[LF_RESOLVE] <holder> <name> <sig> (success|fail)` line for every holder lookup. The probe
// captures System.out during main and reports the measured phase's lines on stderr; the stdout
// line `trace lines in measured phase` counts them (0 when the flag is absent). With the flag,
// lines printed before main (boot) reach stdout raw on both VMs, so that run is read, not
// compared.
//
// Run:
//   javac -d out L4W124LambdaFormHolderHits.java
//   java --add-opens java.base/java.lang.invoke=ALL-UNNAMED --add-opens java.base/java.lang.reflect=ALL-UNNAMED --add-opens java.base/jdk.internal.reflect=ALL-UNNAMED -cp out L4W124LambdaFormHolderHits
//   cratonvm --jdk-only [--nojit] <same --add-opens> -cp out L4W124LambdaFormHolderHits
//   cratonvm --compatible [--nojit] <same --add-opens> -cp out L4W124LambdaFormHolderHits
//   census: add -Djava.lang.invoke.MethodHandle.TRACE_RESOLVE=true, read stderr
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags and -Xint)
// accessor jdk.internal.reflect.DirectConstructorHandleAccessor
// target java.lang.invoke.DirectMethodHandle$Constructor type ()Object
// target form kind DIRECT_NEW_INVOKE_SPECIAL customized false entry holder java.lang.invoke.DirectMethodHandle$Holder.newInvokeSpecial(Object)Object
// newInstance: fresh classes 40, instances ok 80, distinct target forms 1, distinct entries 1, entries not from a holder 0
// newInstance: ()Object form cache: new or changed forms 0, present forms not from a holder 0
// unreflect form kind DIRECT_NEW_INVOKE_SPECIAL customized false entry holder java.lang.invoke.DirectMethodHandle$Holder.newInvokeSpecial(Object)Object
// unreflect: fresh classes 40, instances ok 40, distinct forms 1, distinct entries 1, entries not from a holder 0
// unreflect: ()Object form cache: new or changed forms 0, present forms not from a holder 0
// trace lines in measured phase 0, fail 0
// HOTSPOT_EXPECTED_END
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.io.PrintStream;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.ref.SoftReference;
import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.IdentityHashMap;
import java.util.List;
import java.util.Map;

public class L4W124LambdaFormHolderHits {
    static final int N = 40;
    static int classes;
    static PrintStream out;
    static PrintStream err;

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W124LambdaFormHolderHits.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    /** `public class <name> { public <name>() { super(); } }`, class-file version 52. */
    static byte[] classBytes(String name) throws IOException {
        ByteArrayOutputStream bos = new ByteArrayOutputStream();
        DataOutputStream d = new DataOutputStream(bos);
        d.writeInt(0xCAFEBABE);
        d.writeShort(0);
        d.writeShort(52);
        d.writeShort(10);                       // constant pool count
        d.writeByte(1); d.writeUTF(name);       // #1 Utf8 name
        d.writeByte(7); d.writeShort(1);        // #2 Class #1
        d.writeByte(1); d.writeUTF("java/lang/Object"); // #3
        d.writeByte(7); d.writeShort(3);        // #4 Class #3
        d.writeByte(1); d.writeUTF("<init>");   // #5
        d.writeByte(1); d.writeUTF("()V");      // #6
        d.writeByte(12); d.writeShort(5); d.writeShort(6); // #7 NameAndType
        d.writeByte(10); d.writeShort(4); d.writeShort(7); // #8 Methodref Object.<init>
        d.writeByte(1); d.writeUTF("Code");     // #9
        d.writeShort(0x0021);                   // ACC_PUBLIC | ACC_SUPER
        d.writeShort(2);
        d.writeShort(4);
        d.writeShort(0);                        // interfaces
        d.writeShort(0);                        // fields
        d.writeShort(1);                        // methods
        d.writeShort(0x0001);
        d.writeShort(5);
        d.writeShort(6);
        d.writeShort(1);                        // attributes
        d.writeShort(9);
        d.writeInt(17);
        d.writeShort(1);                        // max_stack
        d.writeShort(1);                        // max_locals
        d.writeInt(5);
        d.write(new byte[] {0x2a, (byte) 0xb7, 0x00, 0x08, (byte) 0xb1});
        d.writeShort(0);                        // exception table
        d.writeShort(0);                        // code attributes
        d.writeShort(0);                        // class attributes
        d.flush();
        return bos.toByteArray();
    }

    static Class<?> fresh(Loader loader) throws Throwable {
        String name = "W124Fresh" + (classes++);
        Class<?> c = loader.define(name, classBytes(name));
        Class.forName(name, true, loader);
        return c;
    }

    static Field field(Class<?> c, String name) throws Exception {
        Field f = c.getDeclaredField(name);
        f.setAccessible(true);
        return f;
    }

    static Method CTOR_ACCESSOR;
    static Field DCHA_TARGET, MH_FORM, LF_KIND, LF_CUSTOMIZED, LF_VMENTRY,
            MN_CLAZZ, MN_NAME, MN_TYPE, MTF_FORMS;
    static Method MT_FORM;

    static void init() throws Exception {
        CTOR_ACCESSOR = Constructor.class.getDeclaredMethod("getConstructorAccessor");
        CTOR_ACCESSOR.setAccessible(true);
        DCHA_TARGET = field(Class.forName("jdk.internal.reflect.DirectConstructorHandleAccessor"), "target");
        MH_FORM = field(MethodHandle.class, "form");
        Class<?> lf = Class.forName("java.lang.invoke.LambdaForm");
        LF_KIND = field(lf, "kind");
        LF_CUSTOMIZED = field(lf, "customized");
        LF_VMENTRY = field(lf, "vmentry");
        Class<?> mn = Class.forName("java.lang.invoke.MemberName");
        MN_CLAZZ = field(mn, "clazz");
        MN_NAME = field(mn, "name");
        MN_TYPE = field(mn, "type");
        MTF_FORMS = field(Class.forName("java.lang.invoke.MethodTypeForm"), "lambdaForms");
        MT_FORM = MethodType.class.getDeclaredMethod("form");
        MT_FORM.setAccessible(true);
    }

    static String shortType(Object t) {
        return String.valueOf(t);
    }

    /** `holder`, `spun`, `fabricated`, `null` or `other`. */
    static String entryClass(Object vmentry) throws Exception {
        if (vmentry == null) {
            return "null";
        }
        Class<?> c = (Class<?>) MN_CLAZZ.get(vmentry);
        if (c == null) {
            return "null";
        }
        if (c.isHidden()) {
            return "spun";
        }
        if (c.getName().equals("java.lang.invoke.LambdaForm")) {
            return "fabricated";
        }
        if (c.getName().endsWith("$Holder")) {
            return "holder";
        }
        return "other";
    }

    static String entryName(Object vmentry) throws Exception {
        if (vmentry == null) {
            return "null";
        }
        Class<?> c = (Class<?>) MN_CLAZZ.get(vmentry);
        String cn = c == null ? "null" : c.getName();
        if (c != null && c.isHidden()) {
            int slash = cn.indexOf('/');
            cn = slash < 0 ? cn : cn.substring(0, slash);
        }
        return cn + "." + MN_NAME.get(vmentry) + shortType(MN_TYPE.get(vmentry));
    }

    static String describeForm(Object form) throws Exception {
        Object vmentry = LF_VMENTRY.get(form);
        return "kind " + LF_KIND.get(form) + " customized " + (LF_CUSTOMIZED.get(form) != null)
                + " entry " + entryClass(vmentry) + " " + entryName(vmentry);
    }

    /** The `()Object` basic type's cached forms, by index (null where empty). */
    @SuppressWarnings("unchecked")
    static Object[] basicForms() throws Exception {
        Object mtf = MT_FORM.invoke(MethodType.methodType(Object.class));
        SoftReference<Object>[] refs = (SoftReference<Object>[]) MTF_FORMS.get(mtf);
        Object[] forms = new Object[refs.length];
        for (int i = 0; i < refs.length; i++) {
            forms[i] = refs[i] == null ? null : refs[i].get();
        }
        return forms;
    }

    static void reportCache(String row, Object[] before, Object[] after) throws Exception {
        int changed = 0;
        int notHolder = 0;
        for (int i = 0; i < after.length; i++) {
            if (after[i] == null) {
                continue;
            }
            boolean isNew = before[i] != after[i];
            String where = entryClass(LF_VMENTRY.get(after[i]));
            if (isNew) {
                changed++;
            }
            if (!where.equals("holder")) {
                notHolder++;
            }
            err.println("[w124] " + row + " ()Object cache [" + i + "]" + (isNew ? " NEW" : "")
                    + " " + describeForm(after[i]));
        }
        out.println(row + ": ()Object form cache: new or changed forms " + changed
                + ", present forms not from a holder " + notHolder);
    }

    static int traceLines(ByteArrayOutputStream capture, int[] fails) {
        String s = capture.toString();
        int n = 0;
        for (String line : s.split("\\R")) {
            if (line.startsWith("[LF_RESOLVE]") || line.startsWith("[SPECIES_RESOLVE]")) {
                n++;
                if (line.endsWith("(fail)")) {
                    fails[0]++;
                }
                err.println("[w124] trace " + line);
            }
        }
        return n;
    }

    public static void main(String[] args) throws Throwable {
        out = System.out;
        err = System.err;
        ByteArrayOutputStream capture = new ByteArrayOutputStream();
        System.setOut(new PrintStream(capture, true));
        init();
        Loader loader = new Loader();
        MethodHandles.Lookup lookup = MethodHandles.lookup();

        // Warm-up: the JDK's first use of reflection and of the handle path, unmeasured.
        for (int i = 0; i < 8; i++) {
            Constructor<?> k = fresh(loader).getConstructor();
            for (int j = 0; j < 3; j++) {
                k.newInstance();
            }
            MethodHandle h = lookup.unreflectConstructor(fresh(loader).getConstructor());
            Object unused = h.invoke();
        }

        int traceTotal = 0;
        int[] traceFails = new int[1];

        // newInstance: getConstructor().newInstance(), twice, per fresh class.
        Object[] before = basicForms();
        capture.reset();
        Map<Object, Boolean> forms = new IdentityHashMap<>();
        Map<Object, Boolean> entries = new IdentityHashMap<>();
        List<Object> targets = new ArrayList<>();
        int ok = 0;
        String accessorName = "none";
        for (int i = 0; i < N; i++) {
            Class<?> c = fresh(loader);
            Constructor<?> k = c.getConstructor();
            Object a = k.newInstance();
            Object b = k.newInstance();
            ok += (a.getClass() == c ? 1 : 0) + (b.getClass() == c && b != a ? 1 : 0);
            Object accessor = CTOR_ACCESSOR.invoke(k);
            if (i == 0) {
                accessorName = accessor == null ? "null" : accessor.getClass().getName();
            }
            if (accessor == null || !DCHA_TARGET.getDeclaringClass().isInstance(accessor)) {
                continue;
            }
            MethodHandle target = (MethodHandle) DCHA_TARGET.get(accessor);
            targets.add(target);
            Object form = MH_FORM.get(target);
            forms.put(form, Boolean.TRUE);
            Object vmentry = LF_VMENTRY.get(form);
            if (vmentry != null) {
                entries.put(vmentry, Boolean.TRUE);
            }
        }
        // Read before anything below prints (a first string concatenation links, and traces).
        Object[] after = basicForms();
        traceTotal += traceLines(capture, traceFails);
        out.println("accessor " + accessorName);
        int notHolder = 0;
        for (Object e : entries.keySet()) {
            if (!entryClass(e).equals("holder")) {
                notHolder++;
            }
        }
        if (!targets.isEmpty()) {
            MethodHandle t0 = (MethodHandle) targets.get(0);
            out.println("target " + t0.getClass().getName() + " type " + t0.type());
            out.println("target form " + describeForm(MH_FORM.get(t0)));
        }
        out.println("newInstance: fresh classes " + N + ", instances ok " + ok + ", distinct target forms "
                + forms.size() + ", distinct entries " + entries.size() + ", entries not from a holder "
                + notHolder);
        reportCache("newInstance", before, after);

        // unreflect: MethodHandles.lookup().unreflectConstructor(k), its first invoke().
        before = basicForms();
        capture.reset();
        forms.clear();
        entries.clear();
        ok = 0;
        MethodHandle first = null;
        for (int i = 0; i < N; i++) {
            Class<?> c = fresh(loader);
            MethodHandle h = lookup.unreflectConstructor(c.getConstructor());
            Object o = h.invoke();
            ok += o.getClass() == c ? 1 : 0;
            if (first == null) {
                first = h;
            }
            Object form = MH_FORM.get(h);
            forms.put(form, Boolean.TRUE);
            Object vmentry = LF_VMENTRY.get(form);
            if (vmentry != null) {
                entries.put(vmentry, Boolean.TRUE);
            }
        }
        after = basicForms();
        traceTotal += traceLines(capture, traceFails);
        notHolder = 0;
        for (Object e : entries.keySet()) {
            if (!entryClass(e).equals("holder")) {
                notHolder++;
            }
        }
        out.println("unreflect form " + describeForm(MH_FORM.get(first)));
        out.println("unreflect: fresh classes " + N + ", instances ok " + ok + ", distinct forms "
                + forms.size() + ", distinct entries " + entries.size() + ", entries not from a holder "
                + notHolder);
        reportCache("unreflect", before, after);

        System.setOut(out);
        out.println("trace lines in measured phase " + traceTotal + ", fail " + traceFails[0]);
    }
}
