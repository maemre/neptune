i = repeat([1;20;-30;0.5;7], inner=10000)
println(sum(abs.(i)))
