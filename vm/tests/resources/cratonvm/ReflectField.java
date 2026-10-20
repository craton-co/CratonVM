// JAVA21+
package cratonvm;

import java.lang.reflect.Field;

/**
 * Phase 88.2: Field get/set on user classes.
 */
public class ReflectField {

    public int intField = 42;
    public String stringField = "hello";
    public static int staticField = 100;
    public final int finalField = 99;

    // 88.2: Get int field via reflection
    public static int testGetIntField() throws Exception {
        ReflectField obj = new ReflectField();
        Field f = ReflectField.class.getDeclaredField("intField");
        Object val = f.get(obj);
        return (Integer) val;  // 42
    }

    // 88.2: Get String field via reflection
    public static int testGetStringField() throws Exception {
        ReflectField obj = new ReflectField();
        Field f = ReflectField.class.getDeclaredField("stringField");
        Object val = f.get(obj);
        return "hello".equals(val) ? 1 : 0;  // 1
    }

    // 88.2: Get/set static field via reflection
    public static int testStaticField() throws Exception {
        Field f = ReflectField.class.getDeclaredField("staticField");
        int before = (Integer) f.get(null);
        f.set(null, 200);
        int after = (Integer) f.get(null);
        // Reset
        f.set(null, 100);
        return before + after;  // 100 + 200 = 300
    }

    // 88.2: Set int field via reflection
    public static int testSetIntField() throws Exception {
        ReflectField obj = new ReflectField();
        Field f = ReflectField.class.getDeclaredField("intField");
        f.set(obj, 123);
        return obj.intField;  // 123
    }
}
