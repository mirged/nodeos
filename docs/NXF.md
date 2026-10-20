# NXF -- the NodeOS eXecutable Format

## Why not ELF

ELF is a *linker's* format. It describes sections for a linker to combine and
segments for a loader to map, and everything an operating system actually needs
to decide is inferred: which segments are really code, what authority the
program will want, whether the bytes on disk are the bytes someone signed.

NXF inverts that. It is a *loader's* format: the decisions a loader must make
are stated in the header rather than deduced.

## Layout

```
+----------------------+ 0
| Header (136 bytes)   |
+----------------------+
| Segment table        |  segment_count * 48
+----------------------+
| Relocation table     |  reloc_count * 24
+----------------------+
| Dependency table     |  dep_count * 16
+----------------------+
| Capability table     |  cap_count * 16
+----------------------+
| Resource envelope    |  72
+----------------------+
| String table         |  NUL-separated UTF-8
+----------------------+
| Segment contents     |  page aligned
+----------------------+
| Signature (optional) |
+----------------------+
```

## The header states

* `magic`, `version`, `header_size` -- structural.
* `arch`, `abi_version` -- an image built against a different kernel ABI is
  refused at load, not at its first bad syscall.
* `entry`, `image_base`, `image_size` -- where it goes and how big it is.
* offsets and counts for every table -- all bounds-checked exactly once, at
  parse time, so the loader can walk them without re-validating.
* `build_id` -- for symbolication and reproducibility checks.
* `checksum` -- CRC-32 over the whole file with the checksum field and the
  signature region excluded, so an image can be signed after the fact without
  invalidating it.

## W^X is a property of the format

`Image::parse` rejects any segment that is both writable and executable. Not the
loader's policy, not a mount option: the parse fails. A malformed image cannot
reach the mapping code at all.

## Capability requirements

```
CapabilityRequirement {
    class          FILE_READ | NETWORK | GPU | DEVICE | ...
    flags          REQUIRED | OPTIONAL
    detail_offset  scope qualifier: a path prefix, a device class, a port range
}
```

This is a **request**, not a grant. The image says what it needs; the launcher
decides what it gets, and the process starts with exactly the handles that
decision produced. `image_flags::STRICT_CAPABILITIES` means "refuse to start if
anything was withheld" rather than degrading.

The boot launcher's policy for the built-in modules is a table in
`kernel/src/launch.rs`, small enough to read in one sitting -- which is the
point. Contrast with a setuid bit, where "what can this program do?" has no
local answer.

## Resource envelope

```
ResourceEnvelope {
    max_memory_bytes, stack_size, heap_reserve,
    cpu_period_ns, cpu_budget_ns,
    max_threads, max_handles, max_channels,
    sched_class, sched_priority,
}
```

Declared by the image, imposed by the launcher, which is free to be stricter and
never obliged to be as permissive. Putting it in the image means the sandbox
exists from the first instruction rather than being configured by whoever
remembers to.

## Packaging

The compiler and linker still emit ELF -- rewriting a code generator to produce
a new container would be a poor use of anyone's time. `nxfpack` translates at
packaging time, which is also the natural place to attach the things ELF has no
vocabulary for:

```sh
nxfpack target/user/release/init -o build/modules/init.nxf \
    --name init --service --strict \
    --cap PROCESS --cap CHANNEL --cap MEMORY --cap CLOCK \
    --cap FILE_READ:optional:/etc \
    --stack 256K --heap 4M --max-memory 64M
```

`nxfpack` re-parses its own output with the exact code the kernel uses before
declaring success. `nxfpack --verify <image.nxf>` does the same for an image
something else wrote: the Maylang compiler emits NXF directly
(`docs/MAYLANG.md`), with no ELF in between. A packer that emits an image the loader rejects is worse than
one that fails, because the failure would surface at boot instead of at build.
