/* A Catallaxy program extended with natives written in C. Build and run:
 *   cargo build --release -p catallaxy-capi
 *   cc -I crates/catallaxy-capi/include crates/catallaxy-capi/examples/ext.c \
 *      target/release/libcatallaxy_capi.a -o ext   (add -lpthread -ldl -lm on Linux)
 *   ./ext
 */
#include <stdio.h>
#include <string.h>

#include "catallaxy.h"

/* native fn c_sum(xs: List[Int]) -> Int */
static cx_status c_sum(cx_vm *vm, const cx_value *args, size_t nargs, cx_value *out, void *user) {
    (void)nargs; (void)user;
    int64_t n = cx_list_len(vm, args[0]), total = 0;
    for (int64_t i = 0; i < n; i++) {
        cx_value item;
        int64_t x;
        if (cx_list_get(vm, args[0], (size_t)i, &item) != 0 || cx_to_int(item, &x) != 0)
            return cx_panic(vm, "c_sum: expected a list of Int");
        total += x;
    }
    *out = cx_int(vm, total);
    return CX_OK;
}

/* native fn c_greet(name: String, punct: String = "!") -> String */
static cx_status c_greet(cx_vm *vm, const cx_value *args, size_t nargs, cx_value *out, void *user) {
    const char *greeting = (const char *)user;
    size_t len = 0, plen = 1;
    const char *name = cx_string_data(args[0], &len);
    const char *punct = "!";
    if (nargs > 1 && !cx_is_missing(args[1])) punct = cx_string_data(args[1], &plen);
    char buf[256];
    int n = snprintf(buf, sizeof buf, "%s, %.*s%.*s", greeting, (int)len, name, (int)plen, punct);
    *out = cx_string(vm, buf, (size_t)n);
    return CX_OK;
}

/* native fn c_apply_twice(f: fn(Int) -> Int, x: Int) -> List[Int] */
static cx_status c_apply_twice(cx_vm *vm, const cx_value *args, size_t nargs, cx_value *out, void *user) {
    (void)nargs; (void)user;
    size_t mark = cx_scope_open(vm);
    size_t f = cx_root(vm, args[0]);
    cx_value first, second;
    if (cx_call(vm, cx_rooted(vm, f), &args[1], 1, &first) != CX_OK) return CX_PANIC;
    size_t h = cx_root(vm, first); /* the next call may collect */
    if (cx_call(vm, cx_rooted(vm, f), &first, 1, &second) != CX_OK) return CX_PANIC;
    cx_value items[2] = {cx_rooted(vm, h), second};
    *out = cx_list(vm, items, 2);
    cx_scope_close(vm, mark);
    return CX_OK;
}

/* native fn c_fail(msg: String) -> Int */
static cx_status c_fail(cx_vm *vm, const cx_value *args, size_t nargs, cx_value *out, void *user) {
    (void)nargs; (void)user; (void)out;
    size_t len = 0;
    const char *msg = cx_string_data(args[0], &len);
    char buf[128];
    snprintf(buf, sizeof buf, "from C: %.*s", (int)len, msg);
    return cx_panic(vm, buf);
}

static const char *PROGRAM =
    "native fn c_sum(xs: List[Int]) -> Int\n"
    "native fn c_greet(name: String, punct: String = \"!\") -> String\n"
    "native fn c_apply_twice(f: fn(Int) -> Int, x: Int) -> List[Int]\n"
    "native fn c_fail(msg: String) -> Int\n"
    "puts(c_sum((1..100).to_list()))\n"
    "puts(c_greet(\"Ada\"))\n"
    "puts(c_greet(\"Grace\", \"?\"))\n"
    "puts(c_apply_twice({ |v| [v * 10].sum() + (1..50).to_list().length }, 3))\n"
    "t = spawn { c_fail(\"boom\") }\n"
    "match t.join() {\n"
    "  Ok(v) => puts(v)\n"
    "  Err(e) => puts(\"caught: #{e}\")\n"
    "}\n";

int main(void) {
    if (!cx_allocator_active()) {
        fprintf(stderr, "the library was built without its global allocator\n");
        return 1;
    }
    if (cx_register("c_sum", c_sum, NULL) || cx_register("c_greet", c_greet, (void *)"Hello") ||
        cx_register("c_apply_twice", c_apply_twice, NULL) || cx_register("c_fail", c_fail, NULL)) {
        fprintf(stderr, "registration failed\n");
        return 1;
    }
    return cx_run_source("ext.cx", PROGRAM);
}
