# NodeOS architecture

NodeOS is a research operating system. It is not trying to be a better Unix; it
is trying to find out what an operating system looks like if you refuse the
three assumptions Unix is built on:

1. that authority is a property of *who you are* rather than *what you hold*,
2. that the kernel should implement files, sockets and terminals,
3. that a process is created by cloning the one that already exists.

Everything below follows from refusing those three.

---

## 1. Authority is a handle, not an identity

There is no user id, no group id, no `root`, and no permission bitmask
consulted on a syscall. A process can act on an object if and only if it holds
a **handle** to it, and the handle carries **rights** that say what it may do.

```
Process A                          Kernel
+-------------------+              +--------------------------+
| handle table      |              | Channel  koid=17         |
|  3 -> (obj 17, RW)| -----------> | Vmo      koid=22         |
|  7 -> (obj 22, R )| -----------> | Process  koid=31         |
+-------------------+              +--------------------------+
```

Three properties make this more than a renaming of file descriptors:

**Rights only narrow.** `Handle::derive` intersects; there is no operation
anywhere in the kernel that widens a right. A read-only VMO handle cannot be
turned into a writable mapping, cannot be duplicated into a writable handle, and
cannot be laundered through `memory_protect` -- `MapFlags::required_rights`
computes what the *mapping* needs and the check happens before any page table is
touched.

**Revocation is transitive.** Every handle points at a *grant node*; deriving a
handle creates a child node. Revoking a node kills the whole subtree, wherever
those handles have travelled, without the kernel tracking where they went. The
cost is a short pointer walk on lookup, bounded at 32 links.

```
grant(root)                 revoke here
    |                            |
    +-- grant(service A) <-------+
    |        |
    |        +-- grant(passed to B)   <- dies too
    |
    +-- grant(service C)               <- unaffected
```

**Closed means closed.** A handle value is a slot index plus a generation tag.
Closing a handle bumps the generation, so a stale value fails lookup rather than
naming whatever moved into the slot.

Separately from rights, an executable declares **capability classes** --
`FILE_READ`, `NETWORK`, `GPU` -- in its NXF header. Those are *policy*: a
launcher decides which to satisfy, and satisfying one means handing over a
handle. The kernel never consults a class; it only ever checks rights. Keeping
the two apart is what stops the class list from quietly becoming a new `root`.

---

## 2. The kernel implements nine things

```
Process   AddressSpace   Vmo      Channel   Thread
Timer     Irq            Device   Namespace   Job
```

That is the complete list of kernel object types, and the syscall surface is 24
entry points. There is no `open`, `read`, `write`, `socket`, `ioctl`, `signal`,
`fork` or `errno`.

Filesystems, networking, terminals, device drivers and graphics are **userspace
processes** reached by sending typed messages to a channel. A driver that
crashes is a process that dies; its handles close, its peers observe
`PEER_CLOSED`, and a supervisor restarts it. Nothing about that path is special
-- it is the same mechanism as any other channel closing.

The trade is real and worth stating: a filesystem read in NodeOS is a message
round trip, where in a monolithic kernel it is a function call. The bet is that
the isolation is worth more than the microseconds, and that bulk data should
move by shared memory object anyway (see §5).

---

## 3. Processes are built, not cloned

`fork()` fuses three separate decisions: create an address space, populate it,
and grant it authority. NodeOS separates them.

```
process_create(name, caps)     -> an empty protection domain, zero handles
   ... loader maps segments into it ...
   ... launcher installs exactly the handles it decided to grant ...
process_start(process, entry, stack, bootstrap_channel)
```

A new process begins life holding **one** capability: a bootstrap channel, in
`RDI`. Its own process handle, its address space, its services, its files --
all of it arrives as the first message on that channel. This is why "what can
this program do?" is answerable by reading one message rather than auditing a
kernel's worth of implicit inheritance.

A **thread** is separate from a process on purpose: creating an execution
context and creating a protection domain are different authorities, requiring
different rights (`MANAGE_THREAD` vs `MANAGE_PROCESS`).

---

## 4. Memory is objects, mapped

There is no `brk`, no anonymous `mmap`, no `/dev/mem`. Memory is named by a
**VMO** -- a range of pages that exists independently of any address space --
and address spaces contain **regions**, each a range of virtual pages backed by
one VMO with fixed permissions.

Pages are committed lazily. Mapping a gigabyte costs one `BTreeMap` insert; the
first touch of each page faults, and the fault handler consults the region to
decide whether the access is allowed before it commits anything.

Copy-on-write is a region flag. A COW mapping is installed read-only; the write
fault checks the frame's reference count and either flips the page writable
(sole owner) or copies it (shared). Frame reference counts live in the frame
database, which is what makes both COW and shared memory possible without a
separate ownership scheme.

### The address space is windowed

Unix gives a process one flat arena. NodeOS gives every *kind* of mapping its
own multi-terabyte window:

```
0x0000_0000_0000_0000  null guard (4 MiB, never mapped)
0x0000_0000_0040_0000  program image
0x0000_1000_0000_0000  heap
0x0000_2000_0000_0000  thread stacks (guard page below each)
0x0000_3000_0000_0000  shared-memory grants
0x0000_4000_0000_0000  IPC mappings
0x0000_5000_0000_0000  memory-mapped files
0x0000_6000_0000_0000  device MMIO
0x0000_7000_0000_0000  runtime / TLS
--------------------- non-canonical hole ---------------------
0xFFFF_8000_0000_0000  direct map of physical memory (kernel)
0xFFFF_C000_0000_0000  kernel object arena
0xFFFF_C800_0000_0000  ioremap window
0xFFFF_E000_0000_0000  kernel stacks
0xFFFF_FFFF_8000_0000  kernel image
```

This is not cosmetic. A wild pointer almost always lands in an unmapped hole
rather than in another object; the kind of a mapping is recoverable from its
address alone; and a window can be withheld from a process entirely, so a
program that was never granted the device window has no address at which a
device mapping could exist.

The kernel's own image is mapped `R-X` / `R--` / `RW-NX` -- W^X holds for the
kernel, and the boot-time self-test asserts it on every boot.

---

## 5. IPC is typed, and capabilities ride along

A message is a record, not a byte stream:

```
Message {
    kind          u32     what this means to the protocol
    flags         u32     REQUEST | REPLY | NON_BLOCKING | ERROR | ...
    txid          u64     correlates a reply with its request
    payload       bytes   protocol-defined, <= 64 KiB
    capabilities  [..]    handles, transferred not copied
}
```

`kind` is what makes the system legible: a channel is an endpoint speaking a
protocol whose message types are enumerated, so an unknown kind is a protocol
error rather than a parsing adventure.

Capabilities are **transferred**: the sender's handle is consumed, the
receiver's table gains an equivalent one. That single primitive is how
everything above the kernel is built. "Open a file" is *send a request, receive
a channel*. "Grant shared memory" is *send a VMO handle*.

Bulk data does **not** go in the payload. It goes in a VMO whose handle travels
in the message, so the kernel's work per IPC operation stays bounded -- which is
what lets the IPC path stay fast enough to put a filesystem behind it.

Backpressure is real: a full queue blocks the sender (or fails, with
`NON_BLOCKING`) rather than growing without limit.

---

## 6. The scheduler has classes, not one heuristic

```
RealTime     earliest deadline first, with an enforced CPU budget
Interactive  fixed priority, short quantum, 32 levels
Normal       weighted fair sharing by virtual runtime
Background   runs only when nothing else is runnable
```

Classes are strictly ordered: a runnable thread in a higher class always
preempts a lower one. Within a class the policy differs, because an audio
thread with a 10 ms deadline and a batch compile want opposite things and no
single fairness knob serves both.

Virtual runtime accrues inversely to weight, so a thread with twice the weight
accumulates virtual time at half the rate and therefore receives twice the CPU.
A waking thread's virtual runtime is clamped forward to the queue minimum, so a
thread that slept for a minute does not then monopolise the machine.

Real-time threads have a period and a budget; exceeding the budget demotes the
thread for the rest of its period rather than letting it starve the machine.
Entering the real-time class requires a capability.

### Interrupt discipline

**The kernel runs with interrupts disabled.** Userspace runs with them enabled;
a trap or syscall enters through a gate that clears `IF`, and the kernel does
not set it again until it leaves. The only exceptions are explicit: the idle
loop's architecturally atomic `sti; hlt`, and kernel threads that opt in.

This one decision buys:

* no nested interrupt handlers, so no re-entrancy analysis for every lock;
* no red-zone hazard from an interrupt landing inside a compiler-generated leaf
  function (which matters here -- see `docs/BUILD.md`);
* context switching is not a special case: a switch happens only where the
  kernel chose to allow one.

It costs interrupt latency bounded by the longest kernel operation. For a
microkernel whose operations are "copy a message" and "map a page", that is a
few microseconds. Real-time work is served by **preemption points** -- the
syscall return path and the idle loop -- not by preemptible kernel code.

The IST is used only for faults that mean *the current stack cannot be trusted*:
`#DF`, NMI, `#MC`, `#DB`. Everything else, including `#PF` and every hardware
IRQ, lands on `TSS.RSP0`, the kernel stack of the thread that was interrupted.
That is what makes blocking inside a page fault (demand paging) and preempting
from a timer IRQ possible at all: the trap frame is already on a stack that
belongs to exactly one thread.

---

## 7. Executables declare what they need

See [`NXF.md`](NXF.md). The short version: ELF is a linker's format that an
operating system was later taught to load, and everything a *loader* needs is
inferred from it. NXF states those things instead -- segments with W^X checked
by the format, required and optional capability classes, and a resource
envelope (memory, threads, handles, stack, scheduling class) that the launcher
turns into a sandbox.

The capability list is a **request**. The launcher answers it. An image that
overstates its needs gains nothing; it simply fails when it uses a capability it
was never handed.

---

## 8. Namespaces: isolation by default

A namespace maps names to capabilities. It is how a process discovers anything
it was not handed directly: `/svc/fs`, `/dev/block0`, `/proc/self` are keys in a
table the launcher populated.

There is no global root to fall back to. A name absent from your namespace names
*nothing*, and no amount of privilege makes it resolve, because there is no
privilege -- only handles. Two processes with different namespaces see
genuinely different systems, and `Namespace::derive` lets a parent hand a child
a strictly narrower view.

---

## 9. What is deliberately unfinished

Honesty about the boundary is part of the design document.

* **SMP.** All the per-CPU machinery, IPIs, affinity masks and per-CPU run
  queues exist and are used; application processors are not yet started.
* **Symbolic relocation.** The NXF loader applies `RELATIVE` relocations.
  `ABSOLUTE64` and `PC_RELATIVE32` need an export table from a dependency, and
  the loader refuses them rather than writing a plausible wrong address.
* **Signatures.** The format has a signature record and the loader has the hook;
  CRC-32 integrity checking is implemented, Ed25519 verification is not.
* **Persistent storage.** The design is written down below; only the RAM
  filesystem service is implemented.
* **Timers as objects.** `Timer` is in the object enum; `wait` with a deadline
  currently polls at scheduler granularity rather than arming a timer.

### The persistent filesystem, as designed

A copy-on-write, journalled object store, intended to live behind the same VFS
protocol the RAM filesystem already speaks:

* **Objects, not inodes-in-a-table.** Every object has a 64-bit id and lives in
  a B-tree keyed by `(object_id, offset)`; directories are objects whose
  contents are `(name -> object_id)` records.
* **Extents.** Data is `(logical offset, physical block, length)` triples, so a
  contiguous file costs one record regardless of size.
* **Copy-on-write.** A write allocates a new block and rewrites the path to the
  root. Nothing is ever overwritten in place, so a torn write cannot corrupt
  live data.
* **Snapshots fall out of COW.** A snapshot is a retained root pointer; the
  reference counts on shared extents keep the blocks alive.
* **Checksums.** Every block carries a CRC of its contents and every pointer
  carries the checksum of the block it points at, so corruption is detected at
  the point of use rather than discovered later.
* **Journal.** A small intent log makes multi-block metadata updates atomic
  across a crash; because data blocks are COW, the journal only ever needs to
  cover metadata.
* **Storage drivers are separate processes.** The filesystem service holds a
  channel to a block-device service, not a device capability of its own.
