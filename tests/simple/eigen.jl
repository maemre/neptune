# graph is:
# 0 -> 1,3,4
# 1 -> 2,3
# 2 -> 1
# 3 -> 2
# 4 -> 0,1,2
# adjacency matrix:
adj = [0 1 0 1 1;
       0 0 1 1 0;
       0 1 0 0 0;
       0 0 1 0 0;
       1 1 1 0 0]

n = size(adj)[1]
# indegree of every edge
d = sum(adj, 1)'

laplacian = zeros(size(adj))

for u = 1:n
    for v = 1:n
        laplacian[u,v] = if u == v
            1
        elseif adj[u, v] != 0
            -1 / sqrt(d[u] * d[v])            
        else
            0
        end
    end
end

eigenpairs = eigfact(laplacian)
λ = eigenpairs.values
ξ = eigenpairs.vectors

println("Eigenvalues are: $λ")
println("Eigenvectors are:")
for i in 1:size(λ)[1]
    println(ξ[:, i])
end

if sum(λ) == n
    println("No isolated vertices")
else
    println("There are isolated vertices")
end
