# espcan_br — SLCAN CAN<->USB-serial bridge for the WeAct CAN485 ESP32 board.
#
# The classic ESP32 needs the Xtensa `esp` Rust toolchain. This Makefile makes
# sure the esp toolchain's cargo/rustc are used (Homebrew's cargo, if present,
# would otherwise shadow them) and that the bundled GCC linker is on PATH.
#
# One-time setup:
#   cargo install espup espflash
#   espup install            # installs the `esp` toolchain + Xtensa GCC + clang
#
# Then:
#   make            # build release
#   make flash      # build, flash over USB, open serial monitor
#   make monitor    # just open the serial monitor
#   make clean

SHELL := /bin/bash

# Put the esp toolchain's cargo/rustc first so Homebrew's cargo can't shadow them.
export PATH := $(HOME)/.rustup/toolchains/esp/bin:$(PATH)

# export-esp.sh (written by `espup install`) adds the Xtensa GCC linker + libclang.
ENV := source $(HOME)/export-esp.sh

.PHONY: all build flash monitor size clean

all: build

build:
	$(ENV) && cargo build --release

flash:
	$(ENV) && cargo run --release

monitor:
	espflash monitor

size:
	$(ENV) && xtensa-esp32-elf-size target/xtensa-esp32-none-elf/release/espcan_br

clean:
	cargo clean
