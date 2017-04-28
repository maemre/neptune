#ifndef NEPTUNE_THREADS_H
#define NEPTUNE_THREADS_H

// Opaque type for Thread-Local GC Structures (tl_gcs)
// This includes heap of a thread hence we are moving all information
// related to heap structure to GC's side for more flexibility
typedef void tl_gcs_t;

#endif // NEPTUNE_THREADS_H
