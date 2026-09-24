# NodeOS build orchestration.
#
# The normal path is:
#
#   make                 Build the default ISO.
#   make run-graphic     Boot the interactive console and shell.
#   make test-fs         Boot the headless image and verify filesystem I/O.
#
# Run `make help` for the complete target list and supported overrides. All
# paths below are relative to the repository root.

PROFILE      ?= release
QEMU         ?= qemu-system-x86_64
QEMU_MEM     ?= 512M
QEMU_CPU     ?= qemu64,+pdpe1gb
QEMU_SMP     ?= 1
BUILD        := build
ISO          := $(BUILD)/nodeos.iso
ISODIR       := $(BUILD)/iso
ISO_GRAPHIC  := $(BUILD)/nodeos-graphic.iso
ISODIR_GRAPHIC := $(BUILD)/iso-graphic
KERNEL       := target/kernel/$(PROFILE)/nodeos-kernel
CARGO_FLAGS  := $(if $(filter release,$(PROFILE)),--release,)

# Audio. `AUDIODEV` picks the host backend: `sdl`/`pa`/`alsa` to actually hear
# it, `wav` to write build/audio.wav (which is how `make test-audio` checks the
# driver without a sound card), `none` to run the card with nothing behind it.
AUDIODEV     ?= sdl
AUDIODEV_ARGS ?= $(if $(filter wav,$(AUDIODEV)),wav$(comma)path=$(BUILD)/audio.wav,$(AUDIODEV))
comma        := ,

# Storage. A FAT16 image built on the host by `make disk`, attached as a virtio
# block device -- the `fs` service's whole world.
DISK         ?= $(BUILD)/disk.img
DISK_SIZE_MB ?= 64

# `-boot d` matters more than it looks: the FAT image `make disk` builds has a
# boot signature of its own, and the BIOS will happily boot it instead of the
# CD -- landing on mtools' "this is not a bootable disk" stub with no serial
# output and nothing to explain why.
QEMU_FLAGS := -machine q35 -cpu $(QEMU_CPU) -smp $(QEMU_SMP) -m $(QEMU_MEM) \
              -no-reboot -no-shutdown -boot d \
              -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
              -audiodev $(AUDIODEV_ARGS),id=snd0 -device AC97,audiodev=snd0 \
              -drive file=$(DISK),if=virtio,format=raw

.PHONY: all build help kernel tools user modules iso iso-graphic iso-doom run run-graphic run-doom debug debug-graphic debug-doom test test-audio test-fs test-mouse test-doom-libc disk clean fmt clippy

all: iso

# `build` is a readable alias for the default target.
build: iso

help:
	@printf '%s\n' \
	  'NodeOS build guide' \
	  '' \
	  'Build artifacts:' \
	  '  make / make build    Build the default bootable ISO' \
	  '  make kernel          Build the kernel only' \
	  '  make user            Build all userspace programs' \
	  '  make modules         Package userspace programs as NXF images' \
	  '  make disk            Create a FAT disk with docs and /bin/*.nxf' \
	  '  make iso             Build the headless/test ISO' \
	  '  make iso-graphic     Build the interactive shell ISO' \
	  '  make iso-doom        Build the Doom ISO (requires DOOM_WAD)' \
	  '' \
	  'Run and debug:' \
	  '  make run             Headless QEMU boot with serial output' \
	  '  make run-graphic     QEMU display with console and shell' \
	  '  make run-doom        QEMU display with Doom' \
	  '  make debug           Headless boot and wait for GDB :1234' \
	  '  make debug-graphic   Graphic boot and wait for GDB :1234' \
	  '  make debug-doom      Doom boot and wait for GDB :1234' \
	  '' \
	  'Checks:' \
	  '  make test            Run kernel self-tests' \
	  '  make test-fs         Verify shell filesystem commands and disk output' \
	  '  make test-audio      Verify audio output with QEMU' \
	  '  make test-mouse      Drive the mouse through QEMU and check the pixels' \
	  '  make test-doom-libc  Run the Doom libc host test' \
	  '  make fmt             Format kernel Rust code' \
	  '  make clippy          Run kernel Clippy checks' \
	  '' \
	  'Maintenance:' \
	  '  make clean           Remove generated build and Cargo artifacts' \
	  '' \
	  'Useful options (override with VAR=value):' \
	  '  PROFILE=debug        Use debug instead of release artifacts' \
	  '  QEMU_MEM=1G         Give QEMU 1 GiB of memory' \
	  '  QEMU_SMP=2          Use two virtual CPUs' \
	  '  AUDIODEV=none       Disable host audio output' \
	  '  AUDIODEV=wav        Write build/audio.wav for audio tests' \
	  '  DISK_SIZE_MB=128    Create a 128 MiB filesystem image' \
	  '  DOOM_WAD=/path/...   Select the Doom IWAD file' \
	  '' \
	  'Typical workflows:' \
	  '  make run-graphic     Explore the shell interactively' \
	  '  make test-fs         Check the image with mdir -i build/disk.img ::' \
	  '  make debug-graphic   Boot and attach GDB on :1234'

# --- Build artifacts --------------------------------------------------------

kernel:
	cd kernel && cargo build $(CARGO_FLAGS)

$(KERNEL): kernel

# --- Userspace and NXF packaging -------------------------------------------

tools:
	cd tools && cargo build --release

user:
	cd user && cargo build $(CARGO_FLAGS)

NXFPACK := tools/target/release/nxfpack
USER_DIR := target/user/x86_64-unknown-linux-gnu/$(PROFILE)
MODULE_DIR := $(BUILD)/modules

# name : nxfpack flags
modules: tools user
	@mkdir -p $(MODULE_DIR)
	$(NXFPACK) $(USER_DIR)/init -o $(MODULE_DIR)/init.nxf --name init --service --strict \
		--cap PROCESS --cap CHANNEL --cap MEMORY --cap CLOCK --cap LOAD --cap DEBUG \
		--stack 256K --heap 4M --max-memory 64M
	$(NXFPACK) $(USER_DIR)/compositor -o $(MODULE_DIR)/compositor.nxf --name compositor --service --strict \
		--cap DISPLAY --cap INPUT --cap CHANNEL --cap MEMORY \
		--stack 256K --heap 4M --max-memory 64M
	$(NXFPACK) $(USER_DIR)/console -o $(MODULE_DIR)/console.nxf --name console --service --strict \
		--cap CHANNEL --cap MEMORY \
		--stack 256K --heap 4M --max-memory 32M
	$(NXFPACK) $(USER_DIR)/shell -o $(MODULE_DIR)/shell.nxf --name shell --service --strict \
		--cap PROCESS --cap LOAD --cap DEBUG --cap CHANNEL --cap MEMORY --cap CLOCK --cap FILE_READ --cap FILE_WRITE --cap FILE_CREATE \
		--stack 256K --heap 4M --max-memory 16M
	$(NXFPACK) $(USER_DIR)/audio -o $(MODULE_DIR)/audio.nxf --name audio --service --strict \
		--cap AUDIO --cap DEVICE --cap CHANNEL --cap MEMORY --cap CLOCK \
		--stack 256K --heap 4M --max-memory 32M
	$(NXFPACK) $(USER_DIR)/fs -o $(MODULE_DIR)/fs.nxf --name fs --service --strict \
		--cap FILE_READ --cap FILE_WRITE --cap FILE_CREATE --cap DEVICE --cap CHANNEL --cap MEMORY \
		--stack 256K --heap 8M --max-memory 64M
	$(NXFPACK) $(USER_DIR)/doom -o $(MODULE_DIR)/doom.nxf --name doom --strict \
		--cap CHANNEL --cap MEMORY --cap CLOCK --cap AUDIO \
		--stack 1M --heap 2M --max-memory 128M

# --- ISO images -------------------------------------------------------------

iso: $(ISO)

iso-graphic: $(ISO_GRAPHIC)

$(ISO): kernel modules grub/grub.cfg
	@mkdir -p $(ISODIR)/boot/grub
	cp $(KERNEL) $(ISODIR)/boot/nodeos-kernel
	cp grub/grub.cfg $(ISODIR)/boot/grub/grub.cfg
	cp $(MODULE_DIR)/*.nxf $(ISODIR)/boot/
	grub-mkrescue -o $(ISO) $(ISODIR) -d /usr/lib/grub/i386-pc 2>/dev/null

# Same contents as $(ISO), but boots the "resident" GRUB entry (console +
# shell) by default instead of the headless autotest entry -- see
# grub/grub-graphic.cfg.
$(ISO_GRAPHIC): kernel modules grub/grub-graphic.cfg
	@mkdir -p $(ISODIR_GRAPHIC)/boot/grub
	cp $(KERNEL) $(ISODIR_GRAPHIC)/boot/nodeos-kernel
	cp grub/grub-graphic.cfg $(ISODIR_GRAPHIC)/boot/grub/grub.cfg
	cp $(MODULE_DIR)/*.nxf $(ISODIR_GRAPHIC)/boot/
	grub-mkrescue -o $(ISO_GRAPHIC) $(ISODIR_GRAPHIC) -d /usr/lib/grub/i386-pc 2>/dev/null

# --- Doom -------------------------------------------------------------------

# The IWAD is not in this repository: it is somebody else's copyrighted data,
# and at 14 MiB it has no business in git either. Point DOOM_WAD at one --
# shareware doom1.wad works, so does a full doom.wad -- or drop it in the
# default location.
DOOM_WAD     ?= doom_reference/DOOM.WAD
ISO_DOOM     := $(BUILD)/nodeos-doom.iso
ISODIR_DOOM  := $(BUILD)/iso-doom

iso-doom: $(ISO_DOOM)

# A boot of its own rather than another entry on the main ISO: this image
# carries the 14 MiB IWAD, and `make test` should not have to build or read it.
# It boots the compositor with Doom and the shell as two of its clients, side
# by side. That used to be impossible: Doom held the framebuffer and the PS/2
# keyboard itself, so the boot configuration had to choose between it and
# `console`. Neither of them holds a device any more.
$(ISO_DOOM): kernel modules grub/grub-doom.cfg
	@test -f "$(DOOM_WAD)" || { \
	  echo "No IWAD at $(DOOM_WAD)."; \
	  echo "Set DOOM_WAD=/path/to/doom1.wad, or put one there."; \
	  exit 1; }
	@mkdir -p $(ISODIR_DOOM)/boot/grub
	cp $(KERNEL) $(ISODIR_DOOM)/boot/nodeos-kernel
	cp grub/grub-doom.cfg $(ISODIR_DOOM)/boot/grub/grub.cfg
	cp $(MODULE_DIR)/compositor.nxf $(MODULE_DIR)/console.nxf $(MODULE_DIR)/shell.nxf \
	   $(MODULE_DIR)/doom.nxf $(MODULE_DIR)/audio.nxf $(MODULE_DIR)/fs.nxf $(ISODIR_DOOM)/boot/
	grub-mkrescue -o $(ISO_DOOM) $(ISODIR_DOOM) -d /usr/lib/grub/i386-pc 2>/dev/null

# --- Run and debug ----------------------------------------------------------

run-doom: $(ISO_DOOM) $(DISK)
	$(QEMU) $(QEMU_FLAGS) -cdrom $(ISO_DOOM) -serial stdio

debug-doom: $(ISO_DOOM) $(DISK)
	$(QEMU) $(QEMU_FLAGS) -cdrom $(ISO_DOOM) -serial stdio -s -S

# --- The disk ---------------------------------------------------------------
# A FAT16 volume built with mtools, which is what makes the filesystem
# checkable from outside NodeOS: `mdir -i build/disk.img ::` on the host reads
# what the shell wrote, and `mcopy` puts files there for it to read. Delete it
# and `make disk` builds a fresh one; it is not in git for the same reason no
# build output is.
disk: $(DISK)

$(DISK): modules
	@mkdir -p $(BUILD)
	dd if=/dev/zero of=$(DISK) bs=1M count=$(DISK_SIZE_MB) status=none
	mformat -i $(DISK) -v NODEOS ::
	mmd -i $(DISK) ::/docs
	mmd -i $(DISK) ::/bin
	printf 'NodeOS on a real filesystem.\r\nThis file was put here by mtools on the host.\r\n' > $(BUILD)/readme.tmp
	mcopy -i $(DISK) $(BUILD)/readme.tmp ::/README.TXT
	mcopy -i $(DISK) README.md ::/docs/NODEOS.MD
	mcopy -i $(DISK) $(MODULE_DIR)/*.nxf ::/bin/
	@rm -f $(BUILD)/readme.tmp
	@echo "disk: $(DISK) ready ($(DISK_SIZE_MB) MiB FAT16)"

run: $(ISO) $(DISK)
	$(QEMU) $(QEMU_FLAGS) -cdrom $(ISO) -serial stdio -display none

run-graphic: $(ISO_GRAPHIC) $(DISK)
	$(QEMU) $(QEMU_FLAGS) -cdrom $(ISO_GRAPHIC) -serial stdio

debug: $(ISO) $(DISK)
	$(QEMU) $(QEMU_FLAGS) -cdrom $(ISO) -serial stdio -display none -s -S

debug-graphic: $(ISO_GRAPHIC) $(DISK)
	$(QEMU) $(QEMU_FLAGS) -cdrom $(ISO_GRAPHIC) -serial stdio -s -S

# --- Checks -----------------------------------------------------------------

test: $(ISO) $(DISK)
	@scripts/run-tests.sh

# Boot, let Doom make a noise, and check that samples actually reached the
# card. QEMU's `wav` audio backend writes what the AC'97 played to a file, so
# "did the driver work?" is a question about bytes rather than about ears.
test-audio: $(ISO_DOOM) $(DISK)
	@scripts/run-audio-test.sh

# Boot, drive the shell through a file's whole life -- create, write, read
# back, delete -- and then check the result with mtools on the host, which is
# the part that makes it a filesystem test rather than a self-consistency test.
test-fs: $(ISO) $(DISK)
	@scripts/run-fs-test.sh

# The pointer is the one feature a headless boot cannot show, so this target
# boots the graphic ISO with a display and drives the mouse from outside with
# QEMU's monitor. Needs python3 on the host; no other target does.
test-mouse: $(ISO_GRAPHIC) $(DISK)
	@python3 scripts/mouse-test.py

fmt:
	cd kernel && cargo fmt

clippy:
	cd kernel && cargo clippy $(CARGO_FLAGS)

clean:
	rm -rf $(BUILD) target tools/target

# The mini-libc under user/doom/libc is hand-written and Doom fails quietly
# when it is wrong. This links the same sources for the host and checks them
# directly, which is a great deal faster than booting to find out.
test-doom-libc:
	@mkdir -p $(BUILD)
	gcc -O2 -std=gnu99 -Wall -ffreestanding -nostdinc \
	    -isystem $$(gcc -print-file-name=include) -I user/doom/include \
	    -mno-red-zone -fno-pic -fno-stack-protector -mgeneral-regs-only \
	    -fno-asynchronous-unwind-tables -nostdlib -static -Wl,-e,_start \
	    -o $(BUILD)/doom-libc-test \
	    user/doom/tests/libc_test.c user/doom/tests/mem_shim.c user/doom/libc/*.c
	@$(BUILD)/doom-libc-test
