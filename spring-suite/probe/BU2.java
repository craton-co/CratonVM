import org.springframework.beans.BeanUtils;
import java.util.*;
public class BU2 {
  public static class IH1 { private List<Integer> list = new ArrayList<>(); public List<Integer> getList(){return list;} public void setList(List<Integer> l){this.list=l;} }
  public static class WH2 { private List<?> list = new ArrayList<>(); public List<?> getList(){return list;} public void setList(List<?> l){this.list=l;} }
  public static void main(String[] a){
    IH1 s=new IH1(); s.getList().add(42); WH2 d=new WH2();
    try { BeanUtils.copyProperties(s,d); System.out.println("OK "+d.getList()); }
    catch(Throwable e){ e.printStackTrace(); }
  }
}
