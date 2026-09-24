# Prebuilt image

`nodeos.iso` is a bootable BIOS ISO built from this tree, so you can watch the
system come up without installing a Rust toolchain:

```sh
qemu-system-x86_64 -machine q35 -cpu qemu64 -m 512M \
    -cdrom prebuilt/nodeos.iso -serial stdio -display none -no-reboot
```

It boots the "autotest" GRUB entry, which runs the kernel self-tests, launches
`init` in ring 3, waits for it to exit, and powers the machine off. Everything
appears on the serial console; nothing is written to the VGA text screen.

Rebuild it with `make iso`.
