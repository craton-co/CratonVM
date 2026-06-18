import javax.xml.stream.*;
import java.io.StringReader;
public class Stax {
  static void t(String n, java.util.concurrent.Callable<?> c){ try { System.out.println(n+" = "+c.call()); } catch(Throwable e){ System.out.println(n+" THREW "+e.getClass().getSimpleName()+": "+e.getMessage()); } }
  public static void main(String[] a) throws Exception {
    XMLInputFactory f = XMLInputFactory.newInstance();
    XMLStreamReader r = f.createXMLStreamReader(new StringReader("<a:root xmlns:a='urn:x'><b>hi</b></a:root>"));
    System.out.println("reader class = "+r.getClass().getName());
    t("getNamespaceContext()", () -> r.getNamespaceContext()!=null);
    t("getNamespaceContext().getNamespaceURI('a')", () -> { r.next(); return r.getNamespaceContext().getNamespaceURI("a"); });
    System.out.println("DONE-STAX");
  }
}
