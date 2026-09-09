# `ServiceLoader.loadInstalled` finds nothing, so every service the JDK looks up through the platform loader is silently empty

**Status:** FIXED 2026-09-09 (`claude/jdkonly-svcloader-20260909`). One
locale residual remains and is localised in section 8.
**Applies to:** JDK 21 and JDK 25, `--real-jdk` and `--jdk-only` (measured on
Linux; see "scope" below).
**Severity:** wide. This is not a locale defect. It is a `ServiceLoader` defect
that a locale symptom led to, and it silently empties **every** service the JDK
resolves through the platform class loader.
**Found:** narrowing the `21-windows` strict-corpus `textformat` row.

## 1. The measurement

```
                                        load()                        loadInstalled()
HotSpot 21   LocaleDataMetaInfo         2  [CLDR…, NonBase…]           2  [CLDR…, NonBase…]
HotSpot 21   FileSystemProvider         2  [ZipFSP, JrtFSP]            2  [ZipFSP, JrtFSP]
CratonVM 21  LocaleDataMetaInfo         2  [CLDR…, NonBase…]           0  []
CratonVM 21  FileSystemProvider         2  [ZipFSP, JrtFSP]            0  []
```

`ServiceLoader.load(S)` resolves through the **thread-context (application)**
loader and is correct. `ServiceLoader.loadInstalled(S)` resolves through the
**platform** loader and returns **nothing, for every service tried**. Both
loaders exist and print sane identities:

```
platform loader = jdk.internal.loader.ClassLoaders$PlatformClassLoader@520
system   loader = jdk.internal.loader.ClassLoaders$AppClassLoader@24
```

Nothing throws. An empty `ServiceLoader` iteration is indistinguishable from
"this service genuinely has no providers", so every caller silently takes its
no-provider path.

## 2. How it surfaced: every non-English locale answers US data

`DecimalFormatSymbols.getInstance(Locale.GERMANY)` -- asked for **by name**, so
locale *selection* is not the variable -- returns US separators, because
`LocaleProviderAdapter.getAdapter` hands back `FallbackLocaleProviderAdapter`
instead of the CLDR one, and the fallback carries root/English data only.

The chain from the defect to that symptom:

1. `CLDRLocaleProviderAdapter`'s static initialiser looks up the supplementary
   `LocaleDataMetaInfo` with **`ServiceLoader.loadInstalled`**.
2. It gets nothing, so its `nonBaseMetaInfo` is null.
3. Its supported-locale set is then only what `java.base`'s
   `CLDRBaseLocaleDataMetaInfo` provides: **5** tags.
4. `LocaleProviderAdapter.getAdapter(spi, Locale.GERMANY)` asks each adapter
   whether it supports `de-DE`. CLDR truthfully says no.
5. Control falls to `FALLBACK`, whose US data is returned.

```
                    availableLocales   has a `de` locale
  HotSpot 21        1063               true
  CratonVM 21       5                  false
```

and the five are `[]`, `[en]`, `[en_US]`, `[en_US_#Latn]`, `[en_US_POSIX]` --
exactly `java.base`'s own set. Every layer behaves correctly given its inputs.
Only step 1 is wrong.

## 3. Three theories this refutes, all of them mine, all measured

Recording these because each looked right and each cost a probe.

**Not the CLDR adapter failing to load.** It constructs fine under CratonVM,
`LocaleProviderAdapter.forType` returns a real instance for every `Type`, and
`adapterPreference` is `[CLDR, JRE]` -- identical to HotSpot.

**Not `jdk.localedata` missing from the module graph.** This was written up as
the cause on 2026-09-09 and is WRONG. Measured: the module is in the boot layer,
`ModuleFinder.ofSystem()` sees it, both `CLDRLocaleDataMetaInfo` (from
`jdk.localedata`) and `CLDRBaseLocaleDataMetaInfo` (from `java.base`) load by
name from the right modules, and the boot layer has 69 modules.

**Not the locale data itself being unreadable.** Asked directly, both providers
return byte-identical data on CratonVM and HotSpot -- the CLDR provider hands
back **1058** `AvailableLocales` tags on both.

The reason all three probes looked healthy is that every one of them used
`ServiceLoader.load`. The JDK's own call site uses `loadInstalled`. **A probe
that does not use the same lookup the code under test uses cannot see the
defect**, and will keep reporting that everything is fine.

## 4. Scope -- what is measured and what is not

Measured on Linux, JDK 21 and 25, both modes: `loadInstalled` empty in all of
them. Two services tried (`LocaleDataMetaInfo`, `FileSystemProvider`), both
empty, which is why this is stated as a general defect rather than a locale one.
`java.time.chrono.Chronology` is 0 on BOTH VMs and is therefore a control, not
evidence.

**Not the whole story for the `textformat` row -- RESOLVED in section 8.** This
section used to record an unexplained conflict: the 2026-09-08 Windows
narrowing measured `--real-jdk` correct and `--jdk-only` wrong, while Linux
showed both wrong, and "those cannot both be the whole story". They could not,
and neither was: there were TWO variables stacked. `loadInstalled` is the
dominant one and is mode-independent; removing it unmasks a second that is
specific to `--jdk-only` on JDK 21, at which point the Linux result reproduces
the Windows pattern exactly. The candidate blamed here at the time -- Windows
having a real `HostLocaleProviderAdapter` -- was not needed to explain it.

Also noticed and NOT chased: under `load()`, CratonVM's `JrtFileSystemProvider`
instantiation reports a `NoSuchMethodError` where HotSpot does not. Unrelated to
`loadInstalled`, possibly its own defect.

## 5. Why this is worth more than the row that found it

`loadInstalled` is how the JDK finds *installed extensions* -- providers that
ship with the platform rather than with the application. Everything resolved
that way is currently invisible to CratonVM, and every one of those call sites
fails the same silent way: no exception, just a service that appears to have no
providers. The locale row is the one symptom that happened to reach a gate.

## 6. Reproducing

`probes/LoadInstalled.java` is the direct test and takes one run:

```
cratonvm --add-exports java.base/sun.util.locale.provider=ALL-UNNAMED -cp . LoadInstalled
#   load()          = 2
#   loadInstalled() = 0     <- the defect
```

`probes/LocaleAdapter.java` (the adapter chain), `probes/CldrLocales.java` (the
5-vs-1063 count), `probes/CldrWhich.java` (which five), `probes/LocaleModule.java`
(the module graph, which is healthy) and `probes/LocaleTags.java` (the provider
data, which is healthy) are the narrowing, kept because three of them are the
refutations in section 3.

Keep `en-US` as a control in any locale probe: it reads the same on a healthy
and a broken VM, so a run where the control differs is a broken probe.

## 7. Root cause, located -- and FIXED

`native-builtins/src/jboss_jdkspecific.rs::register_module_in_loader_catalog`
registered **every** module against `ClassLoader.getSystemClassLoader()`. On a
real JVM a module goes in the catalog of ITS OWN loader: `jdk.localedata`,
`jdk.zipfs` and `jdk.charsets` belong to the PLATFORM loader and `java.base` to
the BOOT loader. So `load()` (thread-context = app loader) walked the catalog
everything had been dumped into and worked, while `loadInstalled()` (platform
loader) walked one nothing was ever registered in.

The module-to-loader MAPPING was already correct, which is why this needed
three refutations to reach (`probes/ModuleLoaders.java` is that measurement).

Fixed on `claude/jdkonly-svcloader-20260909` by asking each module for its own
loader, with the boot case routed to `BootLoader.getServicesCatalog()` --
`ServiceLoader` reads boot-module providers from there specifically, so a plain
`Module.getClassLoader()` swap would have silently dropped `java.base`'s own
providers. A failed per-loader lookup degrades to the old system-loader
behaviour rather than to no catalog at all.

### Measured after the fix

```
CratonVM 21 --jdk-only        load()   loadInstalled()
  LocaleDataMetaInfo             2            2          (was 0)
  FileSystemProvider             2            2          (was 0)
  Chronology                     0            0          control, 0 on HotSpot too
```

and the CLDR adapter's supported set is repaired in **every** cell:

```
                            CLDR availableLocales
  HotSpot 21                       1063
  CratonVM 21 --real-jdk           1063     (was 5)
  CratonVM 21 --jdk-only           1063     (was 5)
  CratonVM 25 --real-jdk           1152     (was 5)   1152 is JDK 25's own count
  CratonVM 25 --jdk-only           1152     (was 5)
```

The user-visible symptom is fixed in three of the four cells, matching HotSpot
exactly including `fr-FR`'s narrow no-break space:

```
  DecimalFormatSymbols.getInstance(Locale.GERMANY)   decimal
  HotSpot 21                                         U+002C
  CratonVM 21 --real-jdk                             U+002C   fixed
  CratonVM 21 --jdk-only                             U+002E   STILL WRONG
  CratonVM 25 --real-jdk                             U+002C   fixed
  CratonVM 25 --jdk-only                             U+002C   fixed
```

## 8. The residual, and what it resolves

**`--jdk-only` on JDK 21 still answers US separators**, and it is now precisely
localised: the adapter's supported set is a full 1063 in that cell, so the
adapter KNOWS `de-DE` and the data lookup still comes back US. Whatever is left
is downstream of the supported-locale set, is specific to `--jdk-only`, and is
specific to JDK 21 (the same mode on JDK 25 is correct).

That **resolves the conflict this page previously recorded as unexplained.** The
2026-09-08 Windows narrowing measured `--real-jdk` correct and `--jdk-only`
wrong; the Linux measurement before this fix showed both wrong. Both were true:
there were two variables stacked. Removing the dominant one (`loadInstalled`,
which is mode-independent) unmasks a second one that is mode- and
version-specific -- and the Linux result now reproduces the Windows pattern
exactly. The earlier "cannot both be the whole story" was right; neither was.

**Correction to this page's own framing.** Section 2 attributed the symptom to
`LocaleProviderAdapter.getAdapter` returning `FallbackLocaleProviderAdapter`.
That reading is a correlated observation, not the deciding one: after the fix
the reflective `getAdapter` call in `probes/LocaleAdapter.java` STILL reports
`Fallback` in every cell, including the three where the separators are now
correct. So that probe line does not reflect the path
`DecimalFormatSymbols.getInstance` actually takes, and should not be used as the
verdict. **The verdict is the separator values and the `availableLocales`
count.** Read section 2's chain as the shape of the failure, not as five
measured steps.

## 9. Reproducing

`probes/LoadInstalled.java` is the direct test and takes one run:

```
cratonvm -cp . LoadInstalled
#   load()          = 2
#   loadInstalled() = 2      <- 0 before the fix
```

`probes/CldrLocales.java` (the 1063 count), `probes/LocaleAdapter.java` (the
separators -- read the separators, not the adapter line), `probes/CldrWhich.java`,
`probes/LocaleModule.java` and `probes/LocaleTags.java` (the two refutations that
showed a healthy module graph and healthy provider data).

Keep `en-US` as a control in any locale probe: it reads the same on a healthy
and a broken VM, so a run where the control differs is a broken probe.

Also noticed and NOT chased: under `load()`, CratonVM's `JrtFileSystemProvider`
instantiation reports a `NoSuchMethodError` where HotSpot does not. It does not
appear under `loadInstalled()`. Unrelated, possibly its own defect.

Related: `docs/known-issues/jdk-only/the-first-jdk-21-run-found-the-javalangaccess-carrier-is-pinned-to-system-1-20260908.md`
finding #4, and the `W6-11` diagnosis referenced from `jboss_jdkspecific.rs`,
which is where the app-loader catalog registration came from.
