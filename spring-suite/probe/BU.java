import org.springframework.beans.BeanUtils;
import java.util.*;
public class BU {
  public static class WH1 { private List<?> list = new ArrayList<>(); public List<?> getList(){return list;} public void setList(List<?> l){this.list=l;} }
  public static class WH2 { private List<?> list = new ArrayList<>(); public List<?> getList(){return list;} public void setList(List<?> l){this.list=l;} }
  public static class IH1 { private List<Integer> list = new ArrayList<>(); public List<Integer> getList(){return list;} public void setList(List<Integer> l){this.list=l;} }
  static void t(String n, Runnable r){ try { r.run(); System.out.println(n+" OK"); } catch(Throwable e){ System.out.println(n+" THREW "+e.getClass().getName()+": "+e.getMessage()); } }
  public static void main(String[] a){
    t("copy List<?> -> List<?>", () -> { WH1 s=new WH1(); s.setList(List.of("foo",42)); WH2 d=new WH2(); BeanUtils.copyProperties(s,d); System.out.println("  d.list="+d.getList()); });
    t("copy List<Integer> -> List<?>", () -> { IH1 s=new IH1(); s.getList().add(42); WH2 d=new WH2(); BeanUtils.copyProperties(s,d); System.out.println("  d.list="+d.getList()); });
    System.out.println("DONE-BU");
  }
}
