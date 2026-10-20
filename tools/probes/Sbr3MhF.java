// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.reflect.Field;

/** SBR3 lane mh: boxing identity of reflective / handle int reads (RJdkReflBox field.int). */
public class Sbr3MhF {
    public int i = 7;
    public static int s = 7;
    public long l = 7;

    public static void main(String[] a) throws Throwable {
        Sbr3MhF h = new Sbr3MhF();
        Field f = Sbr3MhF.class.getField("i");
        Object o = f.get(h);
        System.out.println("field.get == valueOf " + (o == Integer.valueOf(7)) + " class " + o.getClass().getName());
        System.out.println("field.getInt " + f.getInt(h));
        Object boxedTwice = f.get(h);
        System.out.println("field.get twice same " + (o == boxedTwice));
        System.out.println("static.get == valueOf " + (Sbr3MhF.class.getField("s").get(null) == Integer.valueOf(7)));
        System.out.println("long.get == valueOf " + (Sbr3MhF.class.getField("l").get(h) == Long.valueOf(7)));
        MethodHandle g = MethodHandles.lookup().unreflectGetter(f);
        Object viaMh = g.invoke(h);
        System.out.println("mh.invoke == valueOf " + (viaMh == Integer.valueOf(7)));
        Object viaExact = (Object) g.asType(g.type().changeReturnType(Object.class)).invokeExact(h);
        System.out.println("mh.asType Object invokeExact == valueOf " + (viaExact == Integer.valueOf(7)));
        Integer viaInteger = (Integer) g.invoke(h);
        System.out.println("mh.invoke Integer == valueOf " + (viaInteger == Integer.valueOf(7)));
        System.out.println("mh.invoke(int)==7 " + ((int) g.invokeExact(h) == 7));
        System.out.println("big " + (Integer.valueOf(1000) == (Object) g.invoke(new Sbr3MhF() {{ i = 1000; }})));
    }
}
