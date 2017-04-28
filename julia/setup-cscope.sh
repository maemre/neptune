#!/bin/bash
JULIA_DIR=/home/michael/Dropbox/ucsb/classes/current-classes/cs263-runtime-systems/project/neptune/julia
cd /
find $JULIA_DIR -name "*.c" -o -name "*.h" -o -name "*.cpp" -o -name "*.hpp" -o -name "*.jl" > $JULIA_DIR/cscope.files
cd $JULIA_DIR
rm -f cscope.*out
cscope -b -q cscope.files
