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
