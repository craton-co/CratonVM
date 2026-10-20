import java.io.*; import java.security.*; import java.util.*;
public class Pp1 { public static void main(String[] a) throws Throwable {
 PermissionCollection pc = new PropertyPermission("a.b.*", "read").newPermissionCollection();
 pc.add(new PropertyPermission("a.b.*", "read,write"));
 ByteArrayOutputStream bo = new ByteArrayOutputStream();
 try (ObjectOutputStream o = new ObjectOutputStream(bo)) { o.writeObject(pc); }
 System.out.println("bytes " + bo.size());
 Object back = new ObjectInputStream(new ByteArrayInputStream(bo.toByteArray())).readObject();
 System.out.println(back.getClass().getName());
 ObjectStreamClass d = ObjectStreamClass.lookup(pc.getClass());
 for (ObjectStreamField f : d.getFields()) System.out.println(f.getName()+" "+f.getTypeCode()+" "+f.getType());
}}
