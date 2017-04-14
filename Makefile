.PHONY: all clean link neptune julia benchmark testjulia cleanneptune cleanjulia

# build type, either 'debug' or 'release'
BUILD_TYPE=debug
export JULIA_BUILD_MODE = $(BUILD_TYPE)

all: link

link: neptune julia
	rm -fr bin
	mkdir bin
	cp julia/usr/bin/julia bin

neptune: neptune/src/*.rs neptune/Cargo.toml
	cd neptune && cargo build --$(BUILD_TYPE)

julia:
	$(MAKE) -C julia

benchmark: benchmarks/*
	$(MAKE) -C benchmarks

testjulia:
	cd julia && $(MAKE) testall

clean: cleanjulia cleanneptune

cleanjulia:
	$(MAKE) -C julia clean

cleanneptune:
	cd neptune && cargo clean
