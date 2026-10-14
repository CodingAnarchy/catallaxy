/* Loads the shared library at run time, as a plugin host would, and runs a program in it. Given
 * two paths (two copies of the library), loads both into the one process: each is a separate
 * Rust `cdylib`, with its own standard library, global allocator and runtime. Build and run:
 *   cargo build --release -p catallaxy-capi
 *   cc -I crates/catallaxy-capi/include crates/catallaxy-capi/examples/dlopen.c -o dlopen
 *      (add -ldl on older Linux)
 *   cp target/release/libcatallaxy_capi.so copy.so
 *   ./dlopen target/release/libcatallaxy_capi.so copy.so
 */
#include <dlfcn.h>
#include <stdio.h>

#include "catallaxy.h"

typedef int (*active_fn)(void);
typedef int (*run_fn)(const char *, const char *);
typedef int (*register_fn)(const char *, cx_native, void *);
typedef cx_value (*int_fn)(cx_vm *, int64_t);

static int_fn make_int[8];

/* native fn lib_id() -> Int: which copy of the library runs the program. */
static cx_status lib_id(cx_vm *vm, const cx_value *args, size_t nargs, cx_value *out, void *user) {
    (void)args; (void)nargs;
    size_t i = (size_t)user;
    *out = make_int[i](vm, (int64_t)i);
    return CX_OK;
}

/* Strings, lists, a dict, garbage for the collector, and a thread with its own heap. */
static const char *PROGRAM =
    "native fn lib_id() -> Int\n"
    "words = (1..20000).to_list().map { |i| \"w#{i % 977}\" }\n"
    "counts: Dict[String, Int] = [:]\n"
    "for w in words { counts[w] = counts.fetch(w, 0) + 1 }\n"
    "t = spawn { (1..50000).to_list().map { |i| [i, i * 2] }.length }\n"
    "n = match t.join() {\n"
    "  Ok(v) => v\n"
    "  Err(_) => -1\n"
    "}\n"
    "puts(\"library #{lib_id()}: #{counts.length} #{n}\")\n";

int main(int argc, char **argv) {
    if (argc < 2 || argc > 8) {
        fprintf(stderr, "usage: dlopen LIB...\n");
        return 2;
    }
    run_fn run[8];
    for (int i = 1; i < argc; i++) {
        void *lib = dlopen(argv[i], RTLD_NOW | RTLD_LOCAL);
        if (!lib) {
            fprintf(stderr, "dlopen: %s\n", dlerror());
            return 1;
        }
        active_fn active = (active_fn)dlsym(lib, "cx_allocator_active");
        register_fn reg = (register_fn)dlsym(lib, "cx_register");
        make_int[i] = (int_fn)dlsym(lib, "cx_int");
        run[i] = (run_fn)dlsym(lib, "cx_run_source");
        if (!active || !reg || !make_int[i] || !run[i]) {
            fprintf(stderr, "dlsym: %s\n", dlerror());
            return 1;
        }
        if (!active()) {
            fprintf(stderr, "%s: the library was built without its global allocator\n", argv[i]);
            return 1;
        }
        if (reg("lib_id", lib_id, (void *)(size_t)i) != 0) {
            fprintf(stderr, "cx_register failed\n");
            return 1;
        }
    }
    /* Each library runs its program after all are loaded, so their runtimes coexist. */
    for (int i = 1; i < argc; i++) {
        int code = run[i]("dlopen.cx", PROGRAM);
        if (code != 0) return code;
    }
    return 0;
}
