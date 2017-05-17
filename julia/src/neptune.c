// This file is part of Neptune.
// Definitions for Neptune on C side to help interaction with Julia.
// All functions exported from here start with "np_jl_"

#include "julia.h"
#include "neptune.h"

void np_jl_set_typeof(void *v, void *t)
{
  jl_set_typeof(v, t);
}

jl_value_t ** np_jl_svec_data(jl_value_t *v) {
  return jl_svec_data(v);
}
