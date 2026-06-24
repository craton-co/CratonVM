import java.io.*; import java.util.*;
public class IsoProbe3 {
  static class Iso extends ClassLoader {
    final Set<String> iso; final String tag;
    Iso(String t, Set<String> i){ super(Iso.class.getClassLoader()); tag=t; iso=i; }
    public String toString(){ return "Iso("+tag+")"; }
    protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
      synchronized(getClassLoadingLock(name)){
        Class<?> c = findLoadedClass(name);
        if(c==null){
          if(isIso(name)){
            try(InputStream is = getResourceAsStream(name.replace('.','/')+".class")){
              if(is==null) throw new ClassNotFoundException(name);
              byte[] b = is.readAllBytes();
              return defineClass(name,b,0,b.length);
            } catch(IOException e){ throw new ClassNotFoundException(name,e); }
          } else c = super.loadClass(name,resolve);
        }
        return c;
      }
    }
    boolean isIso(String n){ for(String s:iso){ if(s.endsWith(".*")){ if(n.startsWith(s.substring(0,s.length()-1))) return true;} else if(s.equals(n)) return true;} return false; }
  }
  public static class Target {}
  // Holder references Target.class via a constant — resolved through Holder's own loader
  public static class Holder implements java.util.function.Supplier<Class<?>> {
    public Class<?> get(){ return Target.class; }
  }
  public static void main(String[] a) throws Exception {
    Set<String> iso = Set.of("IsoProbe3$Target","IsoProbe3$Holder");
    Iso c1 = new Iso("c1", iso), c2 = new Iso("c2", iso);
    @SuppressWarnings("unchecked")
    var h1 = (java.util.function.Supplier<Class<?>>) c1.loadClass("IsoProbe3$Holder").getConstructor().newInstance();
    @SuppressWarnings("unchecked")
    var h2 = (java.util.function.Supplier<Class<?>>) c2.loadClass("IsoProbe3$Holder").getConstructor().newInstance();
    Class<?> t1 = h1.get();  // Target.class resolved via Holder@c1
    Class<?> t2 = h2.get();  // Target.class resolved via Holder@c2
    System.out.println("t1.loader="+t1.getClassLoader()+" t2.loader="+t2.getClassLoader());
    System.out.println("t1==t2 (want false): "+(t1==t2));
    System.out.println("t1.loader==c1 (want true): "+(t1.getClassLoader()==c1));
    System.out.println("t2.loader==c2 (want true): "+(t2.getClassLoader()==c2));
  }
}
