/*
 * crt_stubs.c — supply the two symbols `yuv.lib(cpu_id.obj)` leaves unresolved.
 *
 * `cpu_id.obj` contains libyuv's ARM capability detection (`ArmCpuCaps`), which reads
 * `/proc/cpuinfo` with `fopen`/`fgets`. That object was compiled expecting to import
 * them from a DLL CRT (`__imp_fopen` / `__imp_fgets`), but the static CRT that
 * `yuv.lib` otherwise wants provides only the non-`__imp_` forms, so linking fails:
 *
 *   yuv.lib(cpu_id.obj) : error LNK2019: unresolved external symbol __imp_fgets
 *   yuv.lib(cpu_id.obj) : error LNK2019: unresolved external symbol __imp_fopen
 *
 * `(*(void**)(&name))` defines the function `name` and then hands back its address for
 * the `__imp_name` global — i.e. it synthesises the import thunk. The ARM path is dead
 * code on x64, so the stubs are never actually called; they exist only to satisfy the
 * linker. This is a build-workaround for the probe, not part of the verification.
 */

#include <stdio.h>

typedef FILE* (*fopen_fn)(const char*, const char*);
typedef char* (*fgets_fn)(char*, int, FILE*);

static FILE* stub_fopen(const char* path, const char* mode) {
  (void)path;
  (void)mode;
  return NULL; /* "file not found" — ArmCpuCaps just gives up */
}

static char* stub_fgets(char* buf, int n, FILE* f) {
  (void)buf;
  (void)n;
  (void)f;
  return NULL; /* EOF */
}

static fopen_fn p_fopen = stub_fopen;
static fgets_fn p_fgets = stub_fgets;

__declspec(dllexport) fopen_fn* __imp_fopen = &p_fopen;
__declspec(dllexport) fgets_fn* __imp_fgets = &p_fgets;
