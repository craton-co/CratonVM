import net.bytebuddy.description.method.MethodDescription;
import net.bytebuddy.description.type.TypeDescription;
import net.bytebuddy.dynamic.scaffold.MethodGraph;
import java.util.*;

public class GraphProbe {
  public static void main(String[] a) throws Exception {
    Class<?> k = Class.forName(a.length>0?a[0]:"org.assertj.core.api.IntegerAssert");
    TypeDescription td = TypeDescription.ForLoadedType.of(k);
    MethodGraph.Linked graph = MethodGraph.Compiler.Default.forJavaHierarchy().compile(td);
    List<String> rows = new ArrayList<>();
    for (MethodGraph.Node n : graph.listNodes()) {
      MethodDescription r = n.getRepresentative();
      if (!r.getName().equals("returns") && !r.getName().equals("as") && !r.getName().equals("withAssertionState")) continue;
      rows.add(String.format("NODE sort=%s | rep=%s | bridge=%b | declaredBy=%s | ret=%s | params=%s | tvars=%s | bridges=%s",
          n.getSort(), r.getName()+r.getDescriptor(), r.isBridge(), r.getDeclaringType().asErasure().getName(),
          r.getReturnType(), r.getParameters().asTypeList(), r.getTypeVariables(), n.getMethodTypes()));
    }
    Collections.sort(rows);
    rows.forEach(System.out::println);
  }
}
