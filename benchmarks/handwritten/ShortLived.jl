# Benchmark that creates lots of short lived objects with boxes Objects should
# be reclaimed quickly by the generational collector and full GC shouldn't be
# triggered often.

type Foo
    bar # :: Any
end

type Stats
    mean::Float64
    median::Float64
end

# immortal = repmat([Foo(3)], 1000, 2)

const N_RUNS = 10

const N_OBJECTS = 10_000_000
const N_BIG_OBJ = 10_000
const BIG_SIZE = 10_000

function run_small()
    println("Small objects")

    foo = Foo("")
    sum = 0.0
    for i=1:N_OBJECTS
        if i % rand(1:10) == 0
            foo = Foo(rand(Int32))
        else
            foo = Foo(rand(Float32))
        end
        sum += foo.bar
    end

    println("Dummy print $sum")
end

function run_big()
    println("Big objects")

    stats = Stats(0, 0)
    biggie = zeros(BIG_SIZE)
    for i=1:N_BIG_OBJ
        if i % rand(1:10) == 0
            biggie = rand(Int32, BIG_SIZE)
        else
            biggie = rand(Float32, BIG_SIZE)
        end

        stats.mean += mean(biggie)
        stats.median += median(biggie)
    end

    println("Dummy stats $stats")
end

for i=1:N_RUNS
    run_small()
    run_big()
end

# println("Last use of immortal: $(length(immortal))")
