.PHONY: all clean link neptune-debug neptune-release julia-debug julia-release benchmark testjulia cleanneptune cleanjulia debug

# build type, either 'debug' or 'release'
BUILD_TYPE=debug
export JULIA_BUILD_MODE = $(BUILD_TYPE)

all: link-$(BUILD_TYPE)

debug: link-debug

link-%: neptune-% julia-%
	rm -fr bin
	mkdir bin
	cp julia/usr/bin/julia bin

neptune-debug: neptune/src/*.rs neptune/Cargo.toml
	cd neptune && cargo build

neptune-release: neptune/src/*.rs neptune/Cargo.toml
	cd neptune && cargo build --release

julia-debug:
	$(MAKE) -C julia -j7 debug

julia-release:
	$(MAKE) -C julia -jy

benchmark: benchmarks/*
	$(MAKE) -C benchmarks

testjulia:
	cd julia && $(MAKE) testall

clean: cleanjulia cleanneptune

cleanjulia:
	$(MAKE) -C julia clean

cleanneptune:
	cd neptune && cargo clean
