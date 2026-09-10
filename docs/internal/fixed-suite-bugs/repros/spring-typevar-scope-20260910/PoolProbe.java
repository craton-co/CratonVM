import net.bytebuddy.description.method.MethodDescription;
import net.bytebuddy.description.type.TypeDescription;
import net.bytebuddy.dynamic.scaffold.MethodGraph;
import net.bytebuddy.pool.TypePool;
import java.util.*;

public class PoolProbe {
  static void report(String label, TypeDescription td) {
    MethodGraph.Linked graph = MethodGraph.Compiler.Default.forJavaHierarchy().compile(td);
    for (MethodGraph.Node n : graph.listNodes()) {
      MethodDescription r = n.getRepresentative();
      if (!r.getName().equals("returns")) continue;
      System.out.println(label + " rep=" + r.getName() + r.getDescriptor()
        + " ret=" + r.getReturnType() + " params=" + r.getParameters().asTypeList());
    }
  }
  public static void main(String[] a) throws Exception {
    String n = "org.assertj.core.api.IntegerAssert";
    report("LOADED ", TypeDescription.ForLoadedType.of(Class.forName(n)));
    TypePool pool = TypePool.Default.of(PoolProbe.class.getClassLoader());
    report("POOL   ", pool.describe(n).resolve());
    // Also: reverse the declared-method order artificially is not possible; but
    // print the class-file order that the pool sees for AbstractObjectAssert.returns
    TypeDescription ao = pool.describe("org.assertj.core.api.AbstractObjectAssert").resolve();
    int i = 0;
    for (MethodDescription.InDefinedShape m : ao.getDeclaredMethods()) {
      if (m.getName().equals("returns") || m.getName().equals("as"))
        System.out.println("POOLORDER " + (i) + " " + m.getName() + m.getDescriptor() + " bridge=" + m.isBridge());
      i++;
    }
  }
}
