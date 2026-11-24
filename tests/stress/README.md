# Hostile-userspace stress suite

These tests treat every ring-3 program as an adversary. Each case is one
syscall sequence that a buggy or malicious program could issue, run in its own
fresh QEMU boot, and judged from the serial log.

```
make test-stress                                   # every case
make test-stress CASES="overflow-transfer chan-nest-drop"
tests/stress/run.sh --list                         # names and descriptions
```

`make test-stress` builds `build/stress/stress.nxf`, then `tests/stress/run.sh`
does the following for each case:

1. copies `build/disk.img` to `build/stress/disk.img` and puts `stress.nxf` on
   the copy, so the real disk image is never modified;
2. boots `build/nodeos.iso` with that disk and types
   `run /bin/stress.nxf <case>` at the shell;
3. waits for a verdict, a kernel panic, the guest dying, or a timeout;
4. if the case returned, runs `run /bin/stress.nxf alive` to confirm that the
   machine still schedules, allocates and passes messages.

| Verdict | Meaning |
|---|---|
| **PASS** | The kernel refused the operation, or survived it, and the system still answers the liveness probe. |
| **FAIL** | The weakness reproduced: a kernel panic, a triple fault, a hang, an unresponsive system, or the case detecting a rights or quota bypass. |
| **ERROR** | The case could not set itself up. This is not a verdict on the kernel. |

Logs are kept in `build/stress/<case>.log`. The script exits non-zero unless
every case passes. All cases are expected to **FAIL on the current tree**:
each one documents a live weakness, and its fix should turn it PASS.

The guest is safe to run. Everything happens inside QEMU (KVM if `/dev/kvm` is
usable; set `ACCEL=` to force TCG). The guest has no disk other than the
scratch copy and no network other than user-mode NAT. Quota cases stop at a
threshold instead of draining the machine, and starvation cases stop after
2 s. Only `heap-exhaust-crash` drains the kernel heap on purpose.

---

## The five weaknesses

### 1. Arithmetic overflow on user-controlled values is a one-syscall kernel panic

The kernel is built with `overflow-checks = true` (root `Cargo.toml`), so any
`a + b` on numbers userspace chose panics the whole machine. No capability is
needed: `memory_allocate` is ungated, and every case uses a VMO it created
itself.

| Case | Where |
|---|---|
| `overflow-transfer` | `sys_memory_transfer` → `Vmo::write`/`Vmo::read`: `offset + data.len()` (`kernel/src/mm/vmo.rs`) |
| `overflow-map-len` | `AddressSpace::map` → `align_up(len)`: `len + 4095` (`kernel/src/mm/space.rs`) |
| `overflow-map-hint` | `AddressSpace::map`: `addr + len > end` for a fixed address |
| `overflow-fault-offset` | `map` never checks `vmo_offset + len <= vmo.size()`; the fault path then computes `page - base + vmo_offset` (`handle_fault`) |

**Fix:** validate `offset`/`len` against the object at the syscall boundary
with `checked_add`, and reject `vmo_offset + len > vmo.size()` in `map`. Do
not switch overflow checks off: they are why these bugs show up as panics
instead of silent memory corruption.

### 2. Channel object graphs: recursive teardown and immortal cycles

A queued message owns its handles, and an endpoint owns its queue, so an
endpoint can be kept alive only by a message sitting in another endpoint's
queue.

* `chan-nest-drop` builds a chain of endpoints, each one queued inside the
  next, and closes the outermost one. `Channel::close` → drop message → drop
  `Handle` → drop `Channel` → drop its inbox → … is one level of recursion per
  link. Kernel stacks are 16 KiB (`task/stack.rs`), so a few thousand links
  run into the guard page in ring 0. The same thing happens when the process
  exits with the chain in its handle table.
* `chan-self-cycle` sends endpoint *b* over *a*, which puts *b* into *b*'s own
  queue, then closes *a*. Nothing can ever receive from *b* again, and nothing
  ever drops it. Each iteration strands about 1 KiB of kernel heap. The default
  of 2,000,000 iterations is several times a 512 MiB guest's memory.

**Fix:** tear queues down iteratively (move dropped messages onto a work list
instead of dropping them in place), and refuse to send an endpoint, or a
channel that transitively holds it, into its own peer. As a backstop, put a
cap on nesting depth.

### 3. Thread entry state is never validated

`sys_thread_create` and `sys_process_start` copy `entry` and `stack` straight
into the frame that `enter_user` returns through with `iretq`. A
non-canonical RIP makes `iretq` raise #GP while the CPU is still at CPL 0,
after `swapgs`, so the exception handler sees a kernel-mode fault with the
user GS base loaded.

| Case | Input |
|---|---|
| `thread-noncanon-rip` | `entry = 0x0000_8000_0000_0000` |
| `thread-noncanon-rsp` | valid entry (a stub that exits without touching its stack), `rsp = 0x0000_8000_0000_0000` |

The only authority needed is the process's own self-handle, which
`process_start` installs in slot 1 of every child.

**Fix:** require `entry` and `stack` to be canonical lower-half addresses
(`layout::is_user_range`) in `thread_create` and `process_start`. Returning
`InvalidArgument` turns the case PASS. So does a kernel that survives and
kills the thread.

### 4. No resource accounting: memory and CPU

| Case | What it shows |
|---|---|
| `mem-commit-quota` | Commits 64 MiB through `memory_transfer`. `stress.nxf` declares `--max-memory 16M`, but `max_memory_bytes` is parsed and never enforced, and `memory_allocate` does not check the `MEMORY` class. |
| `vmo-metadata-quota` | Allocates empty VMOs and never touches a page. Each one costs 8 bytes of kernel heap per page of *virtual* size (`Vmo::new_anonymous`'s page vector), up to 4 MiB per object. It stops at 64 MiB pinned. |
| `heap-exhaust-crash` | The same, but it drains the heap completely and then makes ordinary syscalls. Kernel heap and user pages share one frame pool, and most kernel allocations are infallible (`Arc::new`, `Vec::push` in `HandleTable::insert`, `BTreeMap::insert` in `map`), so the next one hits `__rust_alloc_error_handler`'s panic, from whichever process happens to allocate next. |
| `cpu-interactive-starve` | `thread_create` gates only `RealTime`. Any process can create an `Interactive`/31 thread, and classes are strictly ordered, so a spin loop freezes every Normal-class thread on the machine: shell, console, filesystem, network. A Normal-class witness thread measures this. |
| `cpu-rt-unbounded` | `budget_ns = 0` means "never charged" (`sched::tick`, `SchedParams::over_budget`), so a RealTime thread with no budget is unbounded. Today this is reachable by every shell-launched program, because of weakness 5. |

**Fix:** charge VMO metadata, committed pages, handles and regions to the
creating process. Enforce `max_memory_bytes`, or a job-level limit, in
`memory_allocate` and on commit. Make the kernel's allocations on syscall
paths fallible (`try_reserve`, `Arc::try_new` or pre-reserved slabs) so that
exhaustion returns `NoMemory`. Gate `Interactive` above some priority, or give
lower classes a guaranteed minimum share, and reject `RealTime` with
`budget_ns == 0` or `period_ns == 0`.

### 5. Authority leaks

| Case | What it shows |
|---|---|
| `caps-undeclared` | `session::launch` calls `Process::create(path, u32::MAX)`, and the loader only checks that the *required* classes are granted. It never narrows the grant to what the NXF declares, so every shell-launched program gets the shell's full set: PROCESS, LOAD, CLOCK, FILE_*, NETWORK. `stress.nxf` declares only CHANNEL and MEMORY, yet `process_create` succeeds. |
| `rights-map-noread-protect` | A VMO handle duplicated down to `MAP` only can be mapped with no permissions (`required_rights` asks only for MAP). `memory_protect` then accepts `READ`, because its escalation check covers only WRITE and EXECUTE. After that, the kernel itself copies the contents out through `memory_transfer`. |
| `rights-map-noread-direct` | The same mapping, read directly from ring 3. `handle_fault` demand-pages a read fault without checking `READ`, and the PTE it installs (`PRESENT|USER`) is readable. A correct kernel kills the process here. The harness counts that as PASS. |

**Fix:** in `launch`, request the declared classes (or have the loader
intersect `granted` with the image's declarations). In `protect`, reject any
flag the region did not already have, not only WRITE and EXECUTE. In
`handle_fault`, refuse reads on regions without `READ`. Alternatively, make
`memory_map` require `READ` whenever it installs a present mapping.

---

## Interactions and knobs

* `cpu-rt-unbounded` currently reaches the scheduler only through weakness 5.
  Once `caps-undeclared` is fixed it will PASS with `CapabilityDenied`. To
  keep testing the budget fix after that, repackage with CLOCK:
  `make test-stress STRESS_CAPS="--cap CHANNEL --cap MEMORY --cap CLOCK" CASES=cpu-rt-unbounded`
  (and expect `caps-undeclared` to fail in that configuration).
* A number after the case name changes its size, for example
  `run /bin/stress.nxf chan-nest-drop 500` or `chan-self-cycle 100000`.
  The harness always uses the defaults.
* `chan-self-cycle` PASSing means the kernel survived 2 M cycles. Without a
  kernel-memory counter visible to userspace, it cannot prove that nothing at
  all leaked.
* The quota cases assume a 512 MiB guest (`MEMORY=512M`, the default).
* Increase `CASE_TIMEOUT` (default 150 s; the two long cases get three times
  that) when running under TCG.

## Layout

```
tests/stress/
  run.sh          host harness: one QEMU boot per case, verdict from serial log
  guest/          the `stress` ring-3 program (a member of the user/ workspace)
    src/main.rs   every case, with the root-cause notes inline
    link -> ../../../user/link   shared ring-3 linker script
```
