# Repros — CGLIB / generated-subclass loader-id family

Companion probes for
`docs/known-issues/springboot/configproxy-cglib-loaderid-fixed-20260727.md`.

Both need a Spring 7 / Spring Boot 4 classpath. The 10-scenario functional
battery at `<suite>/cratonvm-suite/lib/*.jar` (Linux build host:
`/data/data/spring-boot-tomcat-crossmodule-20260717/cratonvm-suite`) is the
one they were developed against; any equivalent `spring-core` +
`spring-beans` + `spring-context` + `spring-aop` + `spring-expression` +
`spring-jcl` + `micrometer-*` + `jspecify` set works.

```bash
SPRING_CP="$(ls <suite>/cratonvm-suite/lib/*.jar | tr '\n' ':')"
```

## `CglibDiag.java` — the original singleton-cache repro

Prints `SimpleInstantiationStrategy.getCurrentlyInvokedFactoryMethod()` at
each `@Bean` method entry and counts `Engine` constructions. Correct output
ends with `final engineCtor=1` and both `c?.engine==e: true`; the bug printed
`engineCtor=3` with both `false`, because the inter-`@Bean` call on `this`
re-ran the real factory body instead of routing through the container
singleton.

```bash
javac -cp "$SPRING_CP" -d out-diag CglibDiag.java
cratonvm --java-home "$JAVA_HOME" -cp "out-diag:$SPRING_CP" CglibDiag
```

## `LookupForkDiag.java` + `Driver.java` — the fork-loader residual

`Driver` is compiled into its **own** output directory so it can be put on a
child `URLClassLoader`'s URL path instead of the application classpath. It
drives all three generated-subclass paths whose defining loader must follow
the *superclass*'s loader: the `@Configuration` CGLIB enhancement, the
`@Lookup` method-override subclass (over a **package-private** abstract
method), and the concrete-`FactoryBean` wrapper.

```bash
javac -cp "$SPRING_CP" -d out-probe Driver.java
javac -d out-app LookupForkDiag.java

# A: everything app-loaded (regression guard — must keep passing)
cratonvm --java-home "$JAVA_HOME" -cp "out-app:out-probe:$SPRING_CP" \
  LookupForkDiag app

# B: the bean classes are reachable ONLY through a child URLClassLoader
cratonvm --java-home "$JAVA_HOME" -cp "out-app:$SPRING_CP" \
  LookupForkDiag fork out-probe
```

Both runs must print `passed=11 failed=0`, identical to HotSpot. The check
that catches the residual is `fork.lookupSubclassSameLoader`: before the fix
the generated `…$$SpringCGLIB$$LM0` was defined by the *application* loader
while its superclass belonged to the `URLClassLoader`, so the subclass sat in
a different runtime package (JVMS 5.4.4) than the class it extends and its
package-private override of `obtainWidget()` was not an override at all.
Run A stayed green throughout — the defect is invisible while the bean class
is app-loaded, which is why it survived the first fix round.
