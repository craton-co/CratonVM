import org.springframework.core.ResolvableType;
import java.lang.reflect.*;
import java.util.*;
public class RT {
  static class Box<T extends Number> { List<T> items; Map<String,List<T>> m; T val; }
  static class Raw { List rawList; Map rawMap; Vector rawVec; }
  interface Handler<E> { void handle(E e); }
  static class StrHandler implements Handler<String> { public void handle(String s){} }
  static void t(String n, Runnable r){ try { r.run(); System.out.println(n+" OK"); } catch(Throwable e){ System.out.println(n+" THREW "+e.getClass().getName()+": "+e.getMessage()); } }
  public static void main(String[] a) throws Exception {
    t("forField(items).resolveGeneric", () -> System.out.println("  "+ResolvableType.forField(field(Box.class,"items")).resolveGeneric(0)));
    t("forField(m) nested", () -> System.out.println("  "+ResolvableType.forField(field(Box.class,"m")).getGeneric(1).getGeneric(0)));
    t("forField(val) typevar", () -> System.out.println("  "+ResolvableType.forField(field(Box.class,"val")).resolve()));
    t("forField(rawList)", () -> System.out.println("  "+ResolvableType.forField(field(Raw.class,"rawList")).resolveGeneric(0)));
    t("forField(rawVec)", () -> System.out.println("  "+ResolvableType.forField(field(Raw.class,"rawVec")).resolveGeneric(0)));
    t("forClass(StrHandler).as(Handler)", () -> System.out.println("  "+ResolvableType.forClass(StrHandler.class).as(Handler.class).resolveGeneric(0)));
    t("Box.as(Object).getSuperType bounds", () -> System.out.println("  "+ResolvableType.forClass(Box.class).getGeneric(0).resolve()));
    System.out.println("DONE-RT");
  }
  static Field field(Class<?> c, String n) { try { return c.getDeclaredField(n); } catch(Exception e){ throw new RuntimeException(e); } }
}
