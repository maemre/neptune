Problem/Objective:
------------------
* Problem: Julia's GC is single-threaded, stop-the-world
* Objective: Make a parallel garbage collector for Julia (in Rust!)

Contribution/Solution:
----------------------
* Describe the Julia language briefly
* Describe Julia's GC in C approach:
  - Per-thread Heap
  - Stop the world, one thread handles GC
  - Generational (2 generations)
  - Layout of modules/types/etc., considerations when marking and sweeping
  - (Generally, we can information from our design documents and email we sent Chandra at the beginning)
* Include diagram of the main structs they use
* Diagram of how where they put type/tag and gc information

Implementation:
---------------
* Describe briefly what Rust is/why Rust vs C
* Rust FFI, link Rust to C (include Diagram)
  - tricks related to having same-layout data structures
* Multitude of problems related to special C things Julia does that had to be
  emulated in Rust (so not being so Rust-like)
* Parallelization effort:


Findings:
---------
