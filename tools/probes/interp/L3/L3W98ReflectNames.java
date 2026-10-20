import java.lang.invoke.MethodHandles;
import java.util.*;
import java.util.function.*;

/**
 * Interpreter round i4 wave 98, lane L3: class names and nesting as HotSpot 25
 * answers them -- getName / getSimpleName / getCanonicalName / getTypeName /
 * descriptorString for arrays, member, local, anonymous, lambda and hidden
 * classes; isMemberClass / isLocalClass / isAnonymousClass / isHidden /
 * isSynthetic; getEnclosingClass / getDeclaringClass / getEnclosingMethod /
 * getEnclosingConstructor; getNestHost / getNestMembers / isNestmateOf;
 * getDeclaredClasses / getClasses. Hidden-class address suffixes
 * (`/0x...`) are folded to `/ADDR`.
 */
public class L3W98ReflectNames {
    static class Member { class Deep {} interface I {} enum E { A } record R() {} }
    public static class PubMember {}
    private interface PrivIface {}

    final Object anonField = new Object() {};
    final Object localInCtor;

    L3W98ReflectNames() {
        class CtorLocal {}
        localInCtor = new CtorLocal();
    }

    static String fold(String s) {
        return s == null ? "null" : s.replaceAll("([/.])0x[0-9a-f]+", "$1ADDR");
    }

    static String show(Class<?> c) {
        return "name=" + fold(c.getName())
                + " simple=" + fold(c.getSimpleName())
                + " canonical=" + fold(c.getCanonicalName())
                + " type=" + fold(c.getTypeName())
                + " desc=" + fold(c.descriptorString())
                + " member=" + c.isMemberClass()
                + " local=" + c.isLocalClass()
                + " anon=" + c.isAnonymousClass()
                + " hidden=" + c.isHidden()
                + " synthetic=" + c.isSynthetic()
                + " mods=" + Integer.toHexString(c.getModifiers())
                + " enclosingClass=" + n(c.getEnclosingClass())
                + " declaringClass=" + n(c.getDeclaringClass())
                + " enclosingMethod=" + c.getEnclosingMethod()
                + " enclosingCtor=" + c.getEnclosingConstructor()
                + " nestHost=" + n(c.getNestHost());
    }

    static String n(Class<?> c) {
        return c == null ? "null" : fold(c.getName());
    }

    static void row(String label, Class<?> c) {
        System.out.println(label + ": " + show(c));
    }

    public static void main(String[] args) throws Throwable {
        class Local { class InLocal {} }
        Runnable anon = new Runnable() { public void run() {} };
        Runnable lambda = () -> {};
        Supplier<String> mref = String::new;
        Object[] arrAnon = { new Object() {} };

        row("top", L3W98ReflectNames.class);
        row("member", Member.class);
        row("deep", Member.Deep.class);
        row("member-iface", Member.I.class);
        row("member-enum", Member.E.class);
        row("member-record", Member.R.class);
        row("pub-member", PubMember.class);
        row("priv-iface", PrivIface.class);
        row("local", Local.class);
        row("in-local", Local.InLocal.class);
        row("anon", anon.getClass());
        row("anon-field", new L3W98ReflectNames().anonField.getClass());
        row("ctor-local", new L3W98ReflectNames().localInCtor.getClass());
        row("anon-in-array-init", arrAnon[0].getClass());
        row("lambda", lambda.getClass());
        row("mref", mref.getClass());
        row("int[]", int[].class);
        row("String[][]", String[][].class);
        row("member[]", Member.Deep[].class);
        row("local[]", Local[].class);
        row("anon[]", java.lang.reflect.Array.newInstance(anon.getClass(), 0).getClass());
        row("int", int.class);
        row("void", void.class);
        row("map-entry", Map.Entry.class);

        // A hidden class defined from this class's own bytes would clash with
        // its name rules; define a tiny nestmate-free class instead.
        byte[] bytes = hiddenBytes();
        Class<?> hidden = MethodHandles.lookup().defineHiddenClass(bytes, true).lookupClass();
        row("hidden", hidden);
        row("hidden[]", java.lang.reflect.Array.newInstance(hidden, 0).getClass());

        List<String> nest = new ArrayList<>();
        for (Class<?> c : L3W98ReflectNames.class.getNestMembers()) nest.add(c.getName());
        Collections.sort(nest);
        System.out.println("nest members (sorted): " + nest);
        System.out.println("nest members [0]: " + L3W98ReflectNames.class.getNestMembers()[0].getName());
        System.out.println("deep nestmate of top: " + Member.Deep.class.isNestmateOf(L3W98ReflectNames.class));
        System.out.println("lambda nestmate of top: " + lambda.getClass().isNestmateOf(L3W98ReflectNames.class));
        System.out.println("hidden nestmate of top: " + hidden.isNestmateOf(L3W98ReflectNames.class));

        List<String> declared = new ArrayList<>();
        for (Class<?> c : L3W98ReflectNames.class.getDeclaredClasses()) declared.add(c.getName());
        Collections.sort(declared);
        System.out.println("declared classes (sorted): " + declared);
        List<String> pub = new ArrayList<>();
        for (Class<?> c : L3W98ReflectNames.class.getClasses()) pub.add(c.getName());
        Collections.sort(pub);
        System.out.println("public classes (sorted): " + pub);
        System.out.println("Member declared (sorted): " + sortedNames(Member.class.getDeclaredClasses()));
        System.out.println("Local declared: " + sortedNames(Local.class.getDeclaredClasses()));
        System.out.println("anon declared: " + sortedNames(anon.getClass().getDeclaredClasses()));
        System.out.println("package of anon: " + anon.getClass().getPackageName() + " / int[]: " + int[].class.getPackageName()
                + " / String[]: " + String[].class.getPackageName());
        System.out.println("component of String[][]: " + String[][].class.componentType().getName()
                + " arrayType of int: " + int.class.arrayType().getName());
    }

    static String sortedNames(Class<?>[] cs) {
        List<String> out = new ArrayList<>();
        for (Class<?> c : cs) out.add(c.getName());
        Collections.sort(out);
        return out.toString();
    }

    /** Class file of `public final class L3W98ReflectNamesHidden { }` with no members. */
    static byte[] hiddenBytes() {
        java.io.ByteArrayOutputStream bo = new java.io.ByteArrayOutputStream();
        java.io.DataOutputStream o = new java.io.DataOutputStream(bo);
        try {
            o.writeInt(0xCAFEBABE);
            o.writeShort(0);
            o.writeShort(52);
            o.writeShort(5); // cp count
            o.writeByte(1); o.writeUTF("L3W98ReflectNamesHidden"); // #1
            o.writeByte(7); o.writeShort(1);                        // #2 Class
            o.writeByte(1); o.writeUTF("java/lang/Object");        // #3
            o.writeByte(7); o.writeShort(3);                        // #4 Class
            o.writeShort(0x0031); // public final super
            o.writeShort(2);
            o.writeShort(4);
            o.writeShort(0); o.writeShort(0); o.writeShort(0); o.writeShort(0);
        } catch (java.io.IOException e) {
            throw new RuntimeException(e);
        }
        return bo.toByteArray();
    }
}
