# Maylang on NodeOS

[Maylang](https://github.com/mirged/maylang) has a NodeOS target,
`x86_64-nodeos`, and its compiler runs on NodeOS. Programs built on a Linux
host run here, `mayc` compiles programs here, and it rebuilds itself here into
a compiler byte-identical to the one cross-built on Linux.

```
/> mayc /tests/hello.may -o /hello.nxf
/> run /hello.nxf
Hello, Maylang!
Hello, world!
```

Unlike Linux programs (`docs/MUSL.md`), a Maylang program needs no C library
and no packing step: `mayc` writes NXF itself and the program speaks NodeOS's
own protocols. The Maylang side of this is described in Maylang's
`docs/internals/NODEOS.md`.

## Building it

```sh
make maylang          # build mayc for NodeOS from ../maylang into build/maylang
make                  # the disk now carries the compiler
make test-maylang     # run programs and the compiler in QEMU
make test-maylang-selfhost   # also rebuild the compiler on NodeOS
```

`MAYLANG=/path/to/maylang` selects the checkout. Its first build bootstraps the
Maylang compiler from source, which needs Rust and a C compiler.
`scripts/build-maylang.sh` checks every image it builds with
`nxfpack --verify`, which runs the kernel loader's parser and checksum.

On the disk:

| Path | Contents |
| --- | --- |
| `/bin/mayc.nxf` | The compiler. Run it as `mayc`. |
| `/bin/runtime.may`, `native.may`, `prelude.may`, `runtime/` | The compiler's runtime, which it finds next to itself. `runtime/nodeos.may` is the NodeOS personality compiled into every program. |
| `/stdlib/` | The standard library, found by walking up from the compiler. |
| `/src/mayc/` | The compiler's own source: `mayc /src/mayc/main.may -o /bin/mayc2.nxf`. |
| `/bin/maysmoke.nxf`, `/tests/*.may` | Test programs. |

Compiling a small program needs about 100 MiB of memory; compiling the
compiler needs about 400 MiB, so run the self-build with `QEMU_MEM=1G`. The FAT
driver creates only 8.3 names, so give output files 8.3 names.

## How a Maylang program uses NodeOS

A Maylang program starts like any NodeOS program: its entry point receives the
bootstrap channel in RDI. Maylang's runtime is written against Linux syscall
numbers, so on this target each `syscall` site calls a small gate into an
in-process personality, written in Maylang, which answers with NodeOS
syscalls and protocol messages:

* standard input and output are `CONSOLE_READ_LINE` and `CONSOLE_WRITE_TEXT`
  on the session console; every output line is also logged as
  `may: | <line>`, and exit as `may: exited with N`;
* files and directories are `FS_OPEN`, `FS_READ`, `FS_WRITE`, `FS_LIST` and the
  rest of the filesystem protocol, on the session's filesystem channel;
* memory is `memory_allocate` plus `memory_map` in the heap window;
* `exec` and `system` load an NXF with `process_create`, `process_load` and
  `process_start`, passing the console and filesystem, as the shell does, and
  `wait` reads the child's exit code with `capability_query`.

The program holds nothing it was not handed: the personality is part of the
program, not a server, and uses only the session channels from the bootstrap
message.

## What NodeOS gained for this

`capability_query` reports a terminated process's exit code
(`HandleInfo::exit_code`). This is the exit-code half of K3 in
`docs/MUSL.md`; `process_kill` is still to do.

`nxfpack --verify <image.nxf>` checks an image that `nxfpack` did not write.

## Limits

* No sockets: there is no network service.
* No C externs: there is no dynamic loader.
* No wall clock: time starts at boot.
* A child does not inherit the working directory or environment, because the
  bootstrap message does not carry them; `cd` at the shell does not change
  where a program resolves relative paths (always from `/`).
