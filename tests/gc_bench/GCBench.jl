# The following is the message given in the source files found on http://www.hboehm.info/gc/gc_bench/:
#
# This is adapted from a benchmark written by John Ellis and Pete Kovac
# of Post Communications.
# It was modified by Hans Boehm of Silicon Graphics.
# Translated to C++ 30 May 1997 by William D Clinger of Northeastern Univ.
# Translated to C 15 March 2000 by Hans Boehm, now at HP Labs.
#
# This is no substitute for real applications.  No actual application
# is likely to behave in exactly this way.  However, this benchmark was
# designed to be more representative of real applications than other
# Java GC benchmarks of which we are aware.
# It attempts to model those properties of allocation requests that
# are important to current GC techniques.
# It is designed to be used either to obtain a single overall performance
# number, or to give a more detailed estimate of how collector
# performance varies with object lifetimes.  It prints the time
# required to allocate and collect balanced binary trees of various
# sizes.  Smaller trees result in shorter object lifetimes.  Each cycle
# allocates roughly the same amount of memory.
# Two data structures are kept around during the entire process, so
# that the measured performance is representative of applications
# that maintain some live in-memory data.  One of these is a tree
# containing many pointers.  The other is a large array containing
# double precision floating point numbers.  Both should be of comparable
# size.
#
# The results are only really meaningful together with a specification
# of how much memory was used.  It is possible to trade memory for
# better time performance.  This benchmark should be run in a 32 MB
# heap, though we don't currently know how to enforce that uniformly.
#
# Unlike the original Ellis and Kovac benchmark, we do not attempt
# measure pause times.  This facility should eventually be added back
# in.  There are several reasons for omitting it for now.  The original
# implementation depended on assumptions about the thread scheduler
# that don't hold uniformly.  The results really measure both the
# scheduler and GC.  Pause time measurements tend to not fit well with
# current benchmark suites.  As far as we know, none of the current
# commercial Java implementations seriously attempt to minimize GC pause
# times.

type Node
  left::Nullable{Node}
  right::Nullable{Node}
  i::Int64
  j::Int64
end

Node() = Node(Nullable{Node}(), Nullable{Node}(), 0, 0)
Node(l,r) = Node(l, r, 0, 0)

k_stretch_tree_depth = 18 # about 16Mb
k_long_lived_tree_depth = 16 # about 4Mb
k_array_size = 500000 # about 4Mb
k_min_tree_depth = 4
k_max_tree_depth = 16

# Nodes used by a tree of a given size
function treesize(i::Int64)
  (1 << (i + 1)) - 1
end

# Number of iterations to use for a given tree depth
function numiters(i::Int64)
  convert(Int64, trunc(2 * treesize(k_stretch_tree_depth) / treesize(i)))
end

# Build tree top down, assigning to older objects. 
function populate!(idepth::Int64, thisnode::Node)
  if idepth <= 0
    return Nullable{Node}()
  else
    idepth -= 1
    thisnode.left = Node()
    thisnode.right = Node()
    populate!(idepth, get(thisnode.left))
    populate!(idepth, get(thisnode.right))
  end
end

# Build tree bottom-up
function maketree(idepth::Int64)
  if idepth <= 0
    Node()
  else
    Node(maketree(idepth-1), maketree(idepth-1))
  end
end

# Note: I'm not sure how to do this in Julia, but if you want to see
#       some memory allocation information, start Julia with the option
#       --track-allocation=user, and after the program quits, view the
#       resulting .mem file.
function printdiagnostics()
  #lfreememory = Runtime.getRuntime().freeMemory();
  #ltotalmemory = Runtime.getRuntime().totalMemory();
  #println(" Total memory available = $itotalmemory bytes")
  #println(" Free memory = $lfreememory bytes");
end

function timeconstruction(depth::Int64)
  root = Node()
  inumiters = numiters(depth)

  println("Creating $inumiters trees of depth $depth")
  tic()
  for i in 1:inumiters
    populate!(depth, Node())
  end
  elapsed = toq()
  ms = toms(elapsed)
  println("\tTop down construction took $ms msec")

  tic()
  for i in 1:inumiters
    maketree(depth)
  end
  elapsed = toq()
  ms = toms(elapsed)
  println("\tBottom up construction took $ms msec")
end

function toms(seconds)
  convert(Int64, trunc(seconds * 1000))
end

function main()
  println("Garbage Collector Test")
  println(" Stretching memory with a binary tree of depth $k_stretch_tree_depth")

	printdiagnostics()

  tic()
	# Stretch the memory space quickly
	maketree(k_stretch_tree_depth)

  # Create a long lived object
	println(" Creating a long-lived binary tree of depth $k_long_lived_tree_depth")
	longlivedtree = Node()
	populate!(k_long_lived_tree_depth, longlivedtree)

	# Create long-lived array, filling half of it
	println(" Creating a long-lived array of $k_array_size doubles")
	array = Array{Float64}(k_array_size)
	for i in 1:(k_array_size/2)
    idx = convert(Int64, i)
		array[idx] = 1.0/i
  end
  printdiagnostics()

  for d in k_min_tree_depth : 2 : k_max_tree_depth 
    timeconstruction(d)
  end

  if isnull(longlivedtree) || array[1000] != 1.0/1000
	  println("Failed")
  end

  elapsed = toq()
  ms = toms(elapsed)
  printdiagnostics()
  println("Completed in $ms msec")
end

main()
