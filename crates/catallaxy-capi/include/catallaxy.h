/*
 * catallaxy.h: the C ABI for embedding Catallaxy and writing native extensions in C.
 *
 * Link against the static or shared library built from the `catallaxy-capi` crate
 * (`cargo build --release -p catallaxy-capi` -> libcatallaxy_capi.{a,so,dylib}).
 *
 * Rules (the borrow checker enforces these for Rust extensions; in C they are on you):
 *   - A cx_value is valid during the native call that received or created it, as long as no call
 *     back into the VM (cx_call) happens in between. To keep values across cx_call, root them:
 *         size_t mark = cx_scope_open(vm);
 *         size_t h = cx_root(vm, v);
 *         ... cx_call(...) ...
 *         v = cx_rooted(vm, h);
 *         cx_scope_close(vm, mark);
 *     Allocation alone (cx_int, cx_string, cx_list) never collects.
 *   - A cx_vm is valid only inside the native call it was passed to, on that thread.
 *   - Strings are UTF-8. cx_string copies its input; cx_string_data points into the VM's
 *     immutable string and follows the same validity rule as the value.
 *   - A process runs one program at a time.
 */
#ifndef CATALLAXY_H
#define CATALLAXY_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct cx_vm cx_vm;
typedef uint64_t cx_value;

typedef enum cx_status { CX_OK = 0, CX_PANIC = 1 } cx_status;

/* A native function. Store the result in *out and return CX_OK, or return cx_panic(...). */
typedef cx_status (*cx_native)(cx_vm *vm, const cx_value *args, size_t nargs, cx_value *out, void *user);

/* ---- engine ---------------------------------------------------------------------------- */

/* Registers a native, declared in Catallaxy as `native fn name(...)` (`C.name` for one declared in
 * class C). Returns 0, or -1 if the name is invalid or the 256-native table is full. Call before
 * running a program. */
int cx_register(const char *name, cx_native f, void *user);
/* Sets the program's command-line arguments (`ARGS` in Catallaxy). Call before running. */
void cx_set_args(int argc, const char *const *argv);
/* Compiles and runs a file / source text. Returns the program's exit code; compile errors are
 * printed to stderr and return 2. */
int cx_run_file(const char *path);
int cx_run_source(const char *name, const char *source);
/* 1 if the library's own allocations go through the runtime's allocator, which on out-of-memory
 * releases a reserve, retries, and raises a MemoryError in the program rather than aborting
 * (the default build); 0 if not. The host's malloc is not affected either way. */
int cx_allocator_active(void);

/* ---- values ---------------------------------------------------------------------------- */

cx_value cx_nil(void);
cx_value cx_bool(int b);
cx_value cx_float(double f);
cx_value cx_int(cx_vm *vm, int64_t i);
cx_value cx_string(cx_vm *vm, const char *utf8, size_t len);
cx_value cx_list(cx_vm *vm, const cx_value *items, size_t n);

int cx_is_nil(cx_value v);
int cx_is_bool(cx_value v);
int cx_is_int(cx_value v);
int cx_is_float(cx_value v);
int cx_is_string(cx_value v);
int cx_is_list(cx_value v);
/* True for an optional parameter the caller left out. */
int cx_is_missing(cx_value v);

int cx_truthy(cx_value v);
/* Return 0 on success, -1 if v has another type (or an Int does not fit in int64_t). */
int cx_to_int(cx_value v, int64_t *out);
int cx_to_float(cx_value v, double *out);
/* Returns NULL if v is not a string. Not NUL-terminated. */
const char *cx_string_data(cx_value v, size_t *len);
/* List length and items; -1 if v is not a list (or index out of range). */
int64_t cx_list_len(cx_vm *vm, cx_value v);
int cx_list_get(cx_vm *vm, cx_value v, size_t i, cx_value *out);

/* ---- calls, rooting, errors ------------------------------------------------------------ */

/* Calls a Catallaxy closure. May collect: root any values you hold across it. On CX_PANIC, return
 * CX_PANIC from your native to propagate the panic. */
cx_status cx_call(cx_vm *vm, cx_value closure, const cx_value *args, size_t nargs, cx_value *out);

size_t cx_scope_open(cx_vm *vm);
void cx_scope_close(cx_vm *vm, size_t mark);
size_t cx_root(cx_vm *vm, cx_value v);
cx_value cx_rooted(cx_vm *vm, size_t handle);

/* Records a panic message; `return cx_panic(vm, "...")` from a native raises it in Catallaxy. */
cx_status cx_panic(cx_vm *vm, const char *message);

#ifdef __cplusplus
}
#endif

#endif /* CATALLAXY_H */
