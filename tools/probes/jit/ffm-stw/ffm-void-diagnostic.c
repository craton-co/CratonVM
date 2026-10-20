/* SPDX-License-Identifier: Apache-2.0 */
#ifdef _WIN32
#define FFM_EXPORT __declspec(dllexport)
#else
#define FFM_EXPORT __attribute__((visibility("default")))
#endif

/* The fifth address is deliberately unused and may name only one byte. */
FFM_EXPORT void ffm_void_diagnostic(void (*collect)(void), void *a, void *b,
                                    void *c, void *unused) {
    (void)a; (void)b; (void)c; (void)unused;
    collect();
}
