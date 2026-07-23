# Missing `conf/Catalina/localhost/*.xml` per-webapp context configs — 8 classes

**Not a CratonVM bug.** Fails identically on real JDK 25 (HotSpot) in the
same fixture.

## Symptom

Two flavors, same root cause:

```
org.apache.catalina.LifecycleException: Failed to start component [...]
Caused by: ... /data/data/apps/tomcat/output/build/webapps/manager ...
```
or a plain 404 where 200 was expected:
```
org.junit.ComparisonFailure: expected:<200> but was:<404>
```

## Affected classes

- `org.apache.catalina.manager.TestHostManagerWebapp`
- `org.apache.catalina.manager.TestManagerWebapp`
- `org.apache.catalina.manager.TestManagerWebappSsl`
- `org.apache.catalina.mapper.TestMapperListener`
- `org.apache.catalina.mapper.TestMapperWebapps`
- `org.apache.catalina.servlets.TestDefaultServlet`
- `org.apache.catalina.servlets.TestWebdavServlet`
- `org.apache.tomcat.util.net.TestSsl`

## Root cause

`output/build/conf/Catalina/localhost/` doesn't exist at all in this
fixture (confirmed: `ls` on it 404s). A real `ant deploy` run stages
per-webapp context XML fragments there — `manager.xml`, `host-manager.xml` —
which set `docBase`, restrict access via a nested `Valve`
(`RemoteAddrValve`), and register the webapp as its own `Host`-scoped
context rather than a bare directory under `webapps/`. Without these, the
`manager`/`host-manager` webapps either fail their `StandardContext` startup
outright, or requests that depend on context-scoped mapping/routing 404.
The earlier fixture fix in
`docs/internal/fixed-suite-bugs/tomcat/16-full-suite-6shard-rerun-20260721.md`
only copied the top-level `conf/*.xml` files (`server.xml`, `web.xml`, etc.),
not this per-`Host` subdirectory.

## Fix

Get `conf/Catalina/localhost/manager.xml` and `.../host-manager.xml` from a
real `ant deploy` run in a from-scratch Tomcat build, and copy them into
`output/build/conf/Catalina/localhost/` in the fixture (creating the
directory). If a real `ant deploy` isn't convenient, hand-author minimal
equivalents — check upstream Tomcat's `conf/Catalina/localhost/manager.xml`
(shipped in the binary distribution) as the canonical reference; it's a
small `<Context>` element with `docBase`, `privileged="true"`, and a
`RemoteAddrValve` restricting to `127\.\d+\.\d+\.\d+|::1|0:0:0:0:0:0:0:1`.

## Verify

Rerun the 8 classes under HotSpot first; all should go from
FAIL/LifecycleException/404 to PASS once the context configs are in place.
