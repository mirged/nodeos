# NodeOS build orchestration.
#
#   make            build the kernel and a bootable ISO
#   make run        boot the ISO in QEMU with the console on stdout
#   make debug      same, but wait for gdb on :1234
#   make test       run the in-kernel self-test suite and check the result
#   make clean

PROFILE      ?= release
QEMU         ?= qemu-system-x86_64
QEMU_MEM     ?= 512M
QEMU_CPU     ?= qemu64,+pdpe1gb
QEMU_SMP     ?= 1
BUILD        := build
ISO          := $(BUILD)/nodeos.iso
ISODIR       := $(BUILD)/iso
KERNEL       := target/kernel/$(PROFILE)/nodeos-kernel
CARGO_FLAGS  := $(if $(filter release,$(PROFILE)),--release,)

QEMU_FLAGS := -machine q35 -cpu $(QEMU_CPU) -smp $(QEMU_SMP) -m $(QEMU_MEM) \
              -no-reboot -no-shutdown \
              -device isa-debug-exit,iobase=0xf4,iosize=0x04

.PHONY: all kernel iso run run-graphic debug test clean fmt clippy

all: iso

kernel:
	cd kernel && cargo build $(CARGO_FLAGS)

$(KERNEL): kernel

iso: $(ISO)

$(ISO): kernel $(wildcard boot/grub.cfg)
	@mkdir -p $(ISODIR)/boot/grub
	cp $(KERNEL) $(ISODIR)/boot/nodeos-kernel
	cp grub/grub.cfg $(ISODIR)/boot/grub/grub.cfg
	@for m in $(MODULES); do cp $$m $(ISODIR)/boot/; done
	grub-mkrescue -o $(ISO) $(ISODIR) -d /usr/lib/grub/i386-pc 2>/dev/null

run: $(ISO)
	$(QEMU) $(QEMU_FLAGS) -cdrom $(ISO) -serial stdio -display none

debug: $(ISO)
	$(QEMU) $(QEMU_FLAGS) -cdrom $(ISO) -serial stdio -display none -s -S

test: $(ISO)
	@scripts/run-tests.sh

fmt:
	cd kernel && cargo fmt

clippy:
	cd kernel && cargo clippy $(CARGO_FLAGS)

clean:
	rm -rf $(BUILD) target
