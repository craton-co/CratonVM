import java.lang.reflect.*;
import java.util.*;

/** Sweep 11: array IDENTITY. G69-1 found class_id_of_object(array) is the COMPONENT's. */
public class Sweep11ArrayIdentity {
    static void p(String l, Object v) { System.out.println("A " + l + " = " + v); }
    interface Call { Object get() throws Exception; }
    static void t(String l, Call c) {
        try { p(l, c.get()); }
        catch (Throwable x) { p(l, x.getClass().getName() + " | " + x.getMessage()); }
    }
    static class Inner { }

    public static void main(String[] x) throws Exception {
        int[] i1 = {1, 2};
        int[][] i2 = {{1}, {2}};
        String[] s1 = {"a"};
        String[][] s2 = {{"a"}};
        Inner[] n1 = new Inner[1];
        Object[] o1 = new Object[1];

        // ---- the mirror's own identity ------------------------------------
        t("int[].getName", () -> i1.getClass().getName());
        t("int[][].getName", () -> i2.getClass().getName());
        t("String[].getName", () -> s1.getClass().getName());
        t("String[][].getName", () -> s2.getClass().getName());
        t("Inner[].getName", () -> n1.getClass().getName());
        t("int[].getSimpleName", () -> i1.getClass().getSimpleName());
        t("String[][].getSimpleName", () -> s2.getClass().getSimpleName());
        t("int[].getCanonicalName", () -> i1.getClass().getCanonicalName());
        t("String[][].getCanonicalName", () -> s2.getClass().getCanonicalName());
        t("int[].getTypeName", () -> i1.getClass().getTypeName());
        t("int[].toString", () -> i1.getClass().toString());
        t("String[].toString", () -> s1.getClass().toString());

        // ---- structure ------------------------------------------------------
        t("int[].isArray", () -> i1.getClass().isArray());
        t("int[].componentType", () -> i1.getClass().getComponentType().getName());
        t("int[][].componentType", () -> i2.getClass().getComponentType().getName());
        t("String[][].componentType", () -> s2.getClass().getComponentType().getName());
        t("int[].superclass", () -> i1.getClass().getSuperclass().getName());
        t("int[].interfaces", () -> Arrays.toString(i1.getClass().getInterfaces()));
        t("int[].isPrimitive", () -> i1.getClass().isPrimitive());
        t("int[].modifiers", () -> Modifier.toString(i1.getClass().getModifiers()));

        // ---- identity between two arrays of the same shape -------------------
        t("same_mirror_int", () -> i1.getClass() == new int[3].getClass());
        t("same_mirror_String", () -> s1.getClass() == new String[3].getClass());
        t("int[]_ne_String[]", () -> !i1.getClass().equals(s1.getClass()));
        t("int[]_ne_int[][]", () -> !i1.getClass().equals(i2.getClass()));
        t("literal_matches", () -> i1.getClass() == int[].class);
        t("literal_matches_2d", () -> s2.getClass() == String[][].class);

        // ---- assignability and instanceof ------------------------------------
        t("Object_isInstance", () -> ((Object) i1) instanceof Object);
        t("ObjectArr_isInstance_StringArr", () -> ((Object) s1) instanceof Object[]);
        t("ObjectArr_isInstance_intArr", () -> ((Object) i1) instanceof Object[]);
        t("isAssignable_Str_to_Obj", () -> Object[].class.isAssignableFrom(String[].class));
        t("isAssignable_Obj_to_Str", () -> String[].class.isAssignableFrom(Object[].class));
        t("isInstance_via_mirror", () -> int[].class.isInstance(i1));
        t("cast_ok", () -> Object[].class.cast(s1).length);
        t("cast_bad", () -> Object[].class.cast(i1));

        // ---- forName round trip ----------------------------------------------
        t("forName_intArr", () -> Class.forName("[I").getName());
        t("forName_StrArr", () -> Class.forName("[Ljava.lang.String;").getName());
        t("forName_2d", () -> Class.forName("[[I").getName());
        t("forName_roundtrip", () -> Class.forName(s2.getClass().getName()) == s2.getClass());

        // ---- reflect.Array ----------------------------------------------------
        t("Array_newInstance_name", () -> Array.newInstance(int.class, 2).getClass().getName());
        t("Array_newInstance_2d", () -> Array.newInstance(int.class, 2, 3).getClass().getName());
        t("Array_newInstance_ref", () -> Array.newInstance(Inner.class, 1).getClass().getName());

        // ---- rendering --------------------------------------------------------
        t("valueOf_startsWith", () -> String.valueOf(i1).startsWith("[I@"));
        t("concat_startsWith", () -> ("" + s1).startsWith("[Ljava.lang.String;@"));
        t("deepToString", () -> Arrays.deepToString(i2));
        t("list_of_array", () -> new ArrayList<>(List.of((Object) i1)).toString().startsWith("[[I@"));
        t("array_in_map", () -> {
            Map<String, Object> m = new HashMap<>();
            m.put("k", i1);
            return m.toString().startsWith("{k=[I@");
        });

        // ---- ArrayStoreException names the element type ------------------------
        t("array_store", () -> { Object[] oo = s1; oo[0] = Integer.valueOf(1); return "no throw"; });
        t("array_store_obj", () -> { o1[0] = i1; return "ok"; });

        // ---- clone and equality -------------------------------------------------
        t("clone_class", () -> i1.clone().getClass().getName());
        t("clone_not_same", () -> i1.clone() != i1);
        t("arrays_equals", () -> Arrays.equals(i1, i1.clone()));
    }
}
