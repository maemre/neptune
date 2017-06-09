# Neptune
A mostly concurrent garbage collector for Julia.

## Directory structure

 + `julia/`: A modified version of Julia to work with an external, statically
     linked garbage collector.
 + `neptune/`: Our garbage collector for Julia, written in Rust.
 + `doc/`: Design notes and documents for the project.
 + `benchmarks/`: GC benchmarks.

## Building the project

### Requirements

Neptune is tested only on GNU/Linux working on x86_64 architecture.  We used
the following toolchains and libraries for building, alternatives may not work:

 1. _GCC 5.4.0_ is used for compiling modified version of Julia and linking it
    with the garbage collector. We haven't tried other compilers. Note: this is
    the default version of GCC available on Ubuntu 16.04 LTS.
 2. _Rust 1.18 Nightly_ is used for building Neptune itself. You'll need the
    nightly version since C-style unions are not available as a feature on Rust
    stable yet. You can install it easily using `rustup`. For details,
    see [Rust's website](https://rust-lang.org). You will need both `rustc` and
    `cargo`, both will be installed if you use `rustup`. Cargo will download
    all the dependencies of Neptune.
 3. You will need the libraries required by Julia to build it. See
    `julia/README.md` for details about it.
 4. You will need `pthreads` for threading.
 5. You will need `make` to build both Julia and the whole thing.

### First build

A full build of Julia requires ~1.5 GiB disk space and ~700 MiB virtual memory.
Requirements for Neptune are much less than that, you'd need ~100 MiB extra
disk space tops.  You can make the initial build by running `make` command in
the root directory of the repository.  In all `make` commands, you can pass a
parameter `-jN` to `make` for it to run upto `N` jobs in parallel.  Note that
first build of Julia takes a lot of time, it took ~2 hrs with `make -j4` on an
otherwise idle machine with 32 GB RAM and Haswell i7 processor for us.  In the
meantime, you can take a coffee break or eight then, if you want, design a logo
for the project.

### Incremental builds

Incremental builds take much less time and resources.  If you are modifying
only Neptune, you can have a faster build by executing the following commands
which skip checking Julia and only link it with Neptune after building Neptune:

``` sh cd neptune cargo build --release cd ..  make link ```

### Debug builds

If you want to debug Neptune, you can build it with `cargo build` then
link it with Julia.  To debug Julia itself, follow the guidelines in Julia's
`README.md` file. I.e.
```
$ cd neptune
$ cargo build
$ cd ../julia
$ make debug # this should link it now
```

### Testing the build

Julia comes with its own unit tests, you can execute them by running `make
testall` in `julia` directory.  Tests also give GC statistics about time taken
by GC (both absolute and as percentage) and resident set size.  To run
Neptune's internal tests, run `cargo test` in `neptune` directory.

## Running benchmarks

Run `make benchmark` to run benchmarks after having a successful build.  The
benchmark command _does not_ trigger a new build so you need to make sure that
you built after your changes.

## Using cscope
To use cscope, which allows easier navigation of C source, do the following:
 
  1. (in Ubuntu-based derivatives) $ apt-get install cscope
  2. $ cd /
  3. $ find /path/to/repo/julia -name "*.c" -o "*.h" -o "*.cpp" -o "*.hpp" > /path/to/repo/cscope.files
  4. $ cd /path/to/repo/julia
  5. $ cscope -b -q
  6. If you're using vim, you can tell it where to find your cscope database file with:
     :cscope add /path/to/repo/julia/cscope.out
     (I'm not sure about emacs, but there is most definitely a way someone has created)
  7. You can also run cscope in the command line just with
     $ cscope
     using Ctrl-d to quit.

Ctags is another possible alternative, although my knowledge of how to use it isn't
as high, and I feel like it's not as effective as cscope in some respects.

## Licensing

See LICENSE for licensing details.
