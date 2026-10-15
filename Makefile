# NodeOS build orchestration.
#
# Two commands are all you need:
#
#   make        Build the kernel, the userspace programs, the bootable ISO and
#               the FAT disk image. The disk carries /bin/*.nxf and, when one is
#               present, /DOOM.WAD.
#   make run    Boot the window system in QEMU -- compositor, console and shell,
#               with the disk attached. Type `run /bin/doom.nxf` at the shell.
#
# Everything below is for debugging and CI. `make help` lists it all.
#
# Common overrides (VAR=value):
#   PROFILE=debug      debug-build the kernel and userspace
#   QEMU_MEM=1G        give the guest more memory
#   DOOM_WAD=/path...  package a different IWAD as /DOOM.WAD
#   DISK_SIZE_MB=128   larger filesystem image
#   AUDIODEV=none      keep the host quiet

PROFILE      ?= release
QEMU         ?= qemu-system-x86_64
QEMU_MEM     ?= 512M
QEMU_CPU     ?= qemu64,+pdpe1gb
QEMU_SMP     ?= 1

BUILD        := build
ISODIR       := $(BUILD)/iso
ISO          := $(BUILD)/nodeos.iso
ISODIR_TEST  := $(BUILD)/iso-test
ISO_TEST     := $(BUILD)/nodeos-test.iso
KERNEL       := target/kernel/$(PROFILE)/nodeos-kernel
CARGO_FLAGS  := $(if $(filter release,$(PROFILE)),--release,)

# Audio. `sdl`/`pa`/`alsa` to hear it, `wav` to write build/audio.wav (what
# `make test-audio` uses), `none` to boot with a card and nothing behind it.
AUDIODEV     ?= sdl
comma        := ,
AUDIODEV_ARGS ?= $(if $(filter wav,$(AUDIODEV)),wav$(comma)path=$(BUILD)/audio.wav,$(AUDIODEV))

# Storage: a FAT16 image built on the host with mtools.
DISK         ?= $(BUILD)/disk.img
DISK_SIZE_MB ?= 64
DOOM_WAD     ?= DOOM.WAD

NXFPACK      := tools/target/release/nxfpack
USER_DIR     := target/user/x86_64-unknown-linux-gnu/$(PROFILE)
MODULE_DIR   := $(BUILD)/modules

# `-boot d` matters: the FAT image `make disk` builds has a boot signature of
# its own, and the BIOS would boot it instead of the CD.
QEMU_FLAGS   := -machine q35 -cpu $(QEMU_CPU) -smp $(QEMU_SMP) -m $(QEMU_MEM) \
                -no-reboot -no-shutdown -boot d \
                -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
                -audiodev $(AUDIODEV_ARGS),id=snd0 -device AC97,audiodev=snd0 \
                -drive file=$(DISK),if=virtio,format=raw

.PHONY: all build kernel tools user modules iso disk run debug \
        test test-fs test-apps test-cc test-edit test-audio test-mouse test-doom-libc \
        fmt clippy clean help

# --- What you actually run --------------------------------------------------

all: build
build: iso disk

run: $(ISO) $(DISK)
	$(QEMU) $(QEMU_FLAGS) -cdrom $(ISO) -serial stdio

# --- Build artifacts --------------------------------------------------------

kernel:
	cd kernel && cargo build $(CARGO_FLAGS)

tools:
	cd tools && cargo build --release

user:
	cd user && cargo build $(CARGO_FLAGS)

# Package each userspace program as an NXF, declaring the capability classes it
# requires. Boot services are `--strict`: the launcher refuses a module that
# asks for more than policy allows. The shell-launched programs need no launcher
# policy -- the shell hands them capabilities at runtime.
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
		--cap PROCESS --cap LOAD --cap DEBUG --cap CHANNEL --cap MEMORY --cap CLOCK \
		--cap FILE_READ --cap FILE_WRITE --cap FILE_CREATE --cap AUDIO \
		--stack 256K --heap 4M --max-memory 16M
	$(NXFPACK) $(USER_DIR)/audio -o $(MODULE_DIR)/audio.nxf --name audio --service --strict \
		--cap AUDIO --cap DEVICE --cap CHANNEL --cap MEMORY --cap CLOCK \
		--stack 256K --heap 4M --max-memory 32M
	$(NXFPACK) $(USER_DIR)/fs -o $(MODULE_DIR)/fs.nxf --name fs --service --strict \
		--cap FILE_READ --cap FILE_WRITE --cap FILE_CREATE --cap DEVICE --cap CHANNEL --cap MEMORY \
		--stack 256K --heap 8M --max-memory 64M
	$(NXFPACK) $(USER_DIR)/doom -o $(MODULE_DIR)/doom.nxf --name doom \
		--cap CHANNEL --cap MEMORY --cap CLOCK --cap AUDIO \
		--stack 1M --heap 2M --max-memory 128M
	$(NXFPACK) $(USER_DIR)/sysinfo -o $(MODULE_DIR)/sysinfo.nxf --name sysinfo \
		--cap CHANNEL --cap MEMORY --cap CLOCK --cap FILE_READ \
		--stack 256K --heap 2M --max-memory 16M
	$(NXFPACK) $(USER_DIR)/tree -o $(MODULE_DIR)/tree.nxf --name tree \
		--cap CHANNEL --cap MEMORY --cap FILE_READ \
		--stack 256K --heap 2M --max-memory 16M
	$(NXFPACK) $(USER_DIR)/bench -o $(MODULE_DIR)/bench.nxf --name bench \
		--cap CHANNEL --cap MEMORY --cap CLOCK \
		--stack 256K --heap 2M --max-memory 32M
	$(NXFPACK) $(USER_DIR)/edit -o $(MODULE_DIR)/edit.nxf --name edit \
		--cap CHANNEL --cap MEMORY --cap FILE_READ --cap FILE_WRITE --cap FILE_CREATE \
		--stack 256K --heap 8M --max-memory 32M
	$(NXFPACK) $(USER_DIR)/cc -o $(MODULE_DIR)/cc.nxf --name cc \
		--cap CHANNEL --cap MEMORY --cap FILE_READ \
		--stack 512K --heap 32M --max-memory 64M

# The interactive ISO: every module, and a GRUB menu whose default entry boots
# the window system.
iso: $(ISO)
$(ISO): kernel modules grub/grub.cfg
	@test -f /usr/lib/grub/x86_64-efi/modinfo.sh || { \
	  echo "Missing GRUB UEFI modules. Install grub-efi-amd64-bin and rebuild." >&2; \
	  exit 1; \
	}
	@mkdir -p $(ISODIR)/boot/grub
	cp $(KERNEL) $(ISODIR)/boot/nodeos-kernel
	cp grub/grub.cfg $(ISODIR)/boot/grub/grub.cfg
	cp $(MODULE_DIR)/*.nxf $(ISODIR)/boot/
	grub-mkrescue -o $(ISO) $(ISODIR)

# The headless self-test ISO: just enough to run `init` and exit. Built on
# demand by `make test`.
$(ISO_TEST): kernel modules grub/grub-test.cfg
	@test -f /usr/lib/grub/x86_64-efi/modinfo.sh || { \
	  echo "Missing GRUB UEFI modules. Install grub-efi-amd64-bin and rebuild." >&2; \
	  exit 1; \
	}
	@mkdir -p $(ISODIR_TEST)/boot/grub
	cp $(KERNEL) $(ISODIR_TEST)/boot/nodeos-kernel
	cp grub/grub-test.cfg $(ISODIR_TEST)/boot/grub/grub.cfg
	cp $(MODULE_DIR)/init.nxf $(ISODIR_TEST)/boot/
	grub-mkrescue -o $(ISO_TEST) $(ISODIR_TEST)

disk: $(DISK)
$(DISK): modules README.md
	@mkdir -p $(BUILD)
	dd if=/dev/zero of=$(DISK) bs=1M count=$(DISK_SIZE_MB) status=none
	mformat -i $(DISK) -v NODEOS ::
	mmd -i $(DISK) ::/docs
	mmd -i $(DISK) ::/bin
	mmd -i $(DISK) ::/tests
	printf 'NodeOS on a real filesystem.\r\nThis file was put here by mtools on the host.\r\n' > $(BUILD)/readme.tmp
	mcopy -i $(DISK) $(BUILD)/readme.tmp ::/README.TXT
	mcopy -i $(DISK) README.md ::/docs/NODEOS.MD
	mcopy -i $(DISK) $(MODULE_DIR)/*.nxf ::/bin/
	mcopy -i $(DISK) user/cc/tests/hello.c ::/TESTS/HELLO.C
	@if [ -f "$(DOOM_WAD)" ]; then \
	  mcopy -i $(DISK) "$(DOOM_WAD)" ::/DOOM.WAD; \
	  echo "disk: packaged $(DOOM_WAD) as /DOOM.WAD"; \
	else \
	  echo "disk: note: no $(DOOM_WAD); '/bin/doom.nxf' will have no IWAD to load"; \
	fi
	@rm -f $(BUILD)/readme.tmp
	@echo "disk: $(DISK) ready ($(DISK_SIZE_MB) MiB)"

# --- Debug ------------------------------------------------------------------

debug: $(ISO) $(DISK)
	$(QEMU) $(QEMU_FLAGS) -cdrom $(ISO) -serial stdio -s -S

# --- Checks -----------------------------------------------------------------

# Headless self-tests: the kernel runs them on every boot and powers off with a
# status. This is the fast, no-display check.
test: $(ISO_TEST) $(DISK)
	@scripts/run-tests.sh

# Filesystem: type at the shell, then read the same disk back with mtools.
test-fs: $(ISO) $(DISK)
	@scripts/run-fs-test.sh

# The console apps (sysinfo, tree, bench) run from the shell.
test-apps: $(ISO) $(DISK)
	@scripts/run-apps-test.sh

# The C compiler: run it on a program on the disk and check the output.
test-cc: $(ISO) $(DISK)
	@scripts/run-cc-test.sh

# The text editor: type, save, and read the file back with mtools.
test-edit: $(ISO) $(DISK)
	@scripts/run-edit-test.sh

# Sound: boot, run Doom from the shell, and check the recorded WAV is not
# silence.
test-audio: $(ISO) $(DISK)
	@scripts/run-audio-test.sh

# The pointer and window management, driven through QEMU's monitor.
test-mouse: $(ISO) $(DISK)
	@python3 scripts/mouse-test.py

# Doom's mini-libc, built for the host so it can be checked directly.
test-doom-libc:
	@mkdir -p $(BUILD)
	gcc -O2 -std=gnu99 -Wall -ffreestanding -nostdinc \
	    -isystem $$(gcc -print-file-name=include) -I user/doom/include \
	    -mno-red-zone -fno-pic -fno-stack-protector -mgeneral-regs-only \
	    -fno-asynchronous-unwind-tables -nostdlib -static -Wl,-e,_start \
	    -o $(BUILD)/doom-libc-test \
	    user/doom/tests/libc_test.c user/doom/tests/mem_shim.c user/doom/libc/*.c
	@$(BUILD)/doom-libc-test

# --- Maintenance ------------------------------------------------------------

fmt:
	cd kernel && cargo fmt

clippy:
	cd kernel && cargo clippy $(CARGO_FLAGS)

clean:
	rm -rf $(BUILD) target tools/target

help:
	@printf '%s\n' \
	  'NodeOS' \
	  '' \
	  '  make          Build the kernel, userspace, ISO and disk' \
	  '  make run      Boot the window system in QEMU' \
	  '' \
	  'Debug and CI' \
	  '  make debug         Boot and wait for GDB on :1234' \
	  '  make test          Headless kernel self-tests' \
	  '  make test-fs       Filesystem I/O through the shell' \
	  '  make test-apps     Run the console apps from the shell' \
	  '  make test-cc       Compile and run a C program with cc' \
	  '  make test-edit     Edit and save a file with the editor' \
	  '  make test-audio    Check sound using QEMU'"'"'s wav backend' \
	  '  make test-mouse    Drive the mouse and check the pixels' \
	  '  make test-doom-libc  Run the Doom mini-libc host test' \
	  '  make fmt           Format the kernel' \
	  '  make clippy        Lint the kernel' \
	  '  make clean         Remove build artifacts' \
	  '' \
	  'Overrides: PROFILE=debug QEMU_MEM=1G DOOM_WAD=/path/to/doom.wad' \
	  '           DISK_SIZE_MB=128 AUDIODEV=none'
