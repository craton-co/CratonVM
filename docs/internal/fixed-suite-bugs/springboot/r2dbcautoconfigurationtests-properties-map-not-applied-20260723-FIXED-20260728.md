# R2DBC additional properties map binding - fixed 2026-07-28

## Root cause

`invokeinterface` could dispatch through a vtable slot resolved from the
constant-pool owner rather than from the runtime receiver. This is incorrect
for a covariant default-method bridge. In Spring Boot,
`IterableConfigurationPropertySource.filter(Predicate)` has a bridge with the
parent `ConfigurationPropertySource` return descriptor. Dispatching the parent
default returned `FilteredConfigurationPropertiesSource`, which is not
iterable; `MapBinder` consequently found descendants but bound no map entries.

The VM now validates interface vtable and inline-cache targets against
receiver-rooted maximally-specific default-method resolution and uses the
runtime receiver for real interface dispatch. Synthetic lambda proxies remain
on their SAM dispatch path.

## Validation

Using the complete Spring Boot fixture at
`C:\craton\CratonVM-spring-boot-rerun-20260717\apps\spring-boot` and the
task-specific release executable:

| Mode | Result |
|---|---|
| no-JIT | `SBRUNNER_RESULT tests=25 failed=0 aborted=0 skipped=0 containersFailed=0` |
| JIT | `SBRUNNER_RESULT tests=25 failed=0 aborted=0 skipped=0 containersFailed=0` |

Both runs executed
`module/spring-boot-r2dbc:org.springframework.boot.r2dbc.autoconfigure.R2dbcAutoConfigurationTests`.
The focused forked-loader regression probe also passed in both modes and
observed the iterable filtered source plus `{test=value, another=2}` map
binding.

The originally named fixture at `C:\craton\CratonVM\apps\spring-boot` remains
incomplete (it is missing the Spring Boot antlib build artifact), so it was not
used as a runtime acceptance fixture.
