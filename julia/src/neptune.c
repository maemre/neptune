// This file is part of Neptune.
// Definitions for Neptune on C side to help interaction with Julia.
// All functions exported from here start with "np_jl_"

#include "julia.h"
#include "neptune.h"
#include "gc.h"

void np_jl_set_typeof(void *v, void *t)
{
  jl_set_typeof(v, t);
}

jl_value_t ** np_jl_svec_data(jl_value_t *v) {
  return jl_svec_data(v);
}

int np_jl_field_isptr(jl_datatype_t *st, int i) {
  return jl_field_isptr(st, i);
}

uint32_t np_jl_field_offset(jl_datatype_t *st, int i) {
  return jl_field_offset(st, i);
}

void np_verify_parent(char * const ty, jl_value_t * const obj, jl_value_t * const * const slot, char * const msg) {
  verify_parent2(ty, obj, slot, "%s", msg);
}

const char * np_jl_symbol_name(jl_sym_t *s) {
  return jl_symbol_name(s);
}

void np_corruption_fail(jl_datatype_t *vt)
{
    jl_printf(JL_STDOUT, "GC error (probable corruption) :\n");
    gc_debug_print_status();
    jl_(vt);
    gc_debug_critical_error();
    abort();
}


void np_call_finalizer(void * fin, void *p) {
  ((void (*)(void*))fin)(jl_data_ptr(p));
}

void neptune_setmark_buf(tl_gcs_t *gc, void *buf, uint8_t mark_mode, size_t minsz);

void gc_setmark_buf(jl_ptls_t ptls, void *buf, uint8_t mark_mode, size_t minsz) {
  neptune_setmark_buf(ptls->tl_gcs, buf, mark_mode, minsz);
}
