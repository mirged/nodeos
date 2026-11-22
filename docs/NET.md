# Networking

NodeOS reaches the internet. `curl`, built from upstream source against musl and
liblx, fetches over TCP by name:

```text
/> curl -s http://example.com/
<!doctype html><html lang="en"><head><title>Example Domain</title>...
```

and DHCP configures the interface at boot:

```text
net: e1000 8086:100e, MAC 52:54:00:12:34:56, link up
net: DHCP 10.0.2.15/24 via 10.0.2.2, dns 10.0.2.3
```

## Status

| Piece | Where | State |
| --- | --- | --- |
| e1000 driver: descriptor rings, DMA, link state | `user/net/src/lib.rs` | working |
| TCP/IP: ARP, IPv4, TCP, UDP, DHCP (smoltcp 0.11) | `user/net/src/stack.rs` | working |
| Socket service protocol: open/connect/send/recv/poll/close | `user/net/src/main.rs`, `lib.rs` | working |
| Linux sockets in liblx, and `poll` over them | `user/lx/src/net.rs`, `fs.rs` | working |
| Name resolution: musl's resolver over UDP, `/etc/resolv.conf` synthesized | `user/lx/src/net.rs` | working |
| `netprobe`: the stack's own smoke test | `user/netprobe/` | working |
| `curl`, from upstream source, HTTP over IPv4 | `scripts/build-curl.sh` | working: HTTP only |
| `kirc`, from upstream source, an IRC client over IPv4 TCP | `scripts/build-kirc.sh` | working |
| Listening sockets, IPv6, TLS, ICMP | | not started |
| NIC interrupts | | not started: the driver polls |

`make test-net` checks the stack (DHCP, a TCP fetch from a server on the host, a
DNS lookup for a name on the internet). `make test-curl` checks curl: a file
fetched and written to the disk, and a page fetched from the internet.
`make test-kirc` checks kirc against an IRC server on the host: registration,
a channel join, a message out and the server's lines back.

---

## 1. Where the network lives

```text
   curl, kirc    ordinary Linux programs, built against musl
     |  __lx_syscall(SYS_socket, SYS_connect, SYS_recvfrom, SYS_poll, ...)
   musl          upstream, unchanged here
     |
   liblx         Linux personality, in-process, holds one channel to `net`
     |  NET_SOCKET_OPEN / CONNECT / SEND / RECV / POLL, over a typed channel
   net           the service: smoltcp + the e1000 driver, in ring 3
     |  one Ethernet frame at a time, through a device capability
   e1000
```

Nothing in that stack is in the kernel. The kernel contributes a device
capability, a DMA VMO and the channel ABI it already had. That is the whole
reason networking fits NodeOS as it is: **the authority to use the network is a
channel**, so "what can this process reach?" is still answered by reading its
bootstrap message. A program the shell launched with no network channel finds
`socket()` returning `EAFNOSUPPORT`; there is no ambient network to escape to.

The alternative -- a kernel network stack with a socket API, as Unix has -- was
rejected for the reasons in ARCHITECTURE.md §2: it puts ~20k lines of protocol
code and a second attack surface in ring 0, and it would need its own new class
of kernel object. A service can be killed, restarted and reasoned about, and the
microkernel does not grow.

The cost is one message round trip per socket operation. A receive that blocks
costs one call (the service parks it); a `poll` costs one call for the whole set
of descriptors. A memcpy-heavy workload would notice; a program fetching a page
does not.

## 2. The stack

`user/net` is built on [smoltcp](https://github.com/smoltcp-rs/smoltcp) 0.11,
pinned, with `default-features = false` and only what this needs:

```toml
features = ["medium-ethernet", "proto-ipv4", "proto-dhcpv4",
            "socket-tcp", "socket-udp", "socket-dhcpv4", "alloc"]
```

smoltcp was chosen because it is `no_std`, needs no operating system (a device
that hands it frames and a monotonic clock are the whole contract), and its TCP
is a real state machine with retransmission and windows rather than a socket
shim. IPv6, ICMP, the raw-socket backend and its logging shim are switched off:
IPv6 would double the code for a network that cannot route it here, and ICMP
echo is a diagnostic nobody has asked for yet.

The device is [`E1000Phy`](../user/net/src/stack.rs): an smoltcp `Device` over
the existing polled e1000 driver. smoltcp drives a device by asking for a receive
token and a transmit token and letting their closures touch the bytes; both hold
`RefCell`s into the driver because the tokens borrow it immutably and the service
is single-threaded. The driver copies each frame into a DMA bounce buffer, so
smoltcp never needs to know about physical addresses.

Per TCP socket: 8 KiB of receive ring and 8 KiB of transmit ring, 16 sockets per
service at most. That is a memory bound, not a policy, and it is why a large
download works at all -- a socket with a 1 KiB window would be dominated by the
1 ms poll interval.

**Addressing.** The interface asks DHCP at boot and takes the first lease, then
removes the DHCP socket (there is no renewal yet). If no answer arrives within
four seconds it falls back to QEMU's user-mode network addresses -- `10.0.2.15/24`,
gateway `10.0.2.2`, name server `10.0.2.3` -- and says so in the log. A boot with
no DHCP server is therefore still usable under `make run`, which is what the
tests rely on.

**Polling, not interrupts.** The e1000 has no interrupt path here: the service's
event loop waits on its channels with a 1 ms deadline, and each turn drains the
receive ring and lets smoltcp run its timers. That is simple and it bounds
latency at a millisecond; it also means the stack cannot be starved by a
sleeping service. Interrupts would cut the timer overhead and are the obvious
next improvement.

## 3. The socket protocol

One request/reply per operation, on the channel the process was given. The
service's loop is:

```text
poll the stack (receive, ARP, TCP timers, retransmits)
complete every parked request the stack can now satisfy
wait on the launcher channel and every client channel until a message arrives
  or the stack has a timer to fire
```

| Message | Request | Reply |
| --- | --- | --- |
| `NET_INFO` | -- | MAC, link, configured, address/prefix/gateway/DNS (21 bytes) |
| `NET_SOCKET_OPEN` | kind, port | socket id |
| `NET_SOCKET_CONNECT` | id, addr, port, timeout | status |
| `NET_SOCKET_SEND` | id, addr, port, data | status, bytes accepted |
| `NET_SOCKET_RECV` | id, max, timeout | status, sender, data |
| `NET_SOCKET_POLL` | timeout, `(id, interest)`* | one readiness byte per socket |
| `NET_SOCKET_CLOSE` | id | -- |
| `NET_SOCKET_SHUTDOWN` | id, how | -- |
| `NET_SOCKET_INFO` | id | state, local port, peer |

Timeouts are the mechanism that makes blocking cheap: `u32::MAX` waits until
something happens, `0` does not wait, anything else is a deadline in
milliseconds. A receive that cannot be satisfied is **parked** with its reply
channel and answered later from the loop -- so a program blocking in `recv` costs
one message and no polling, and a `poll` over several sockets costs one message
that the service holds until one of them is ready.

Status bytes, not errnos, are the reply vocabulary for well-formed operations
(`OK`, `AGAIN`, `EOF`, `REFUSED`, `TIMEOUT`, `UNREACHABLE`, `RESET`,
`NOT_CONNECTED`); the `ERROR` flag stays for malformed requests. liblx is the
only thing that turns one into the other, which is where the Linux `errno`
vocabulary belongs.

## 4. Linux sockets in liblx

liblx answers the fifteen socket system calls -- `socket`, `connect`, `bind`,
`sendto`, `recvfrom`, `sendmsg`, `recvmsg`, `shutdown`, `listen`, `getsockname`,
`getpeername`, `setsockopt`, `getsockopt`, `accept`, `socketpair` -- and a socket
is one more kind of object in the descriptor table, beside `File`, `Dir` and the
console. That is what makes `read()` and `write()` work on a socket: musl does
not know a descriptor is a socket, and `printf`ing to one has to reach `send`.

* **`O_NONBLOCK`** is read from the `socket(2)` type word, from `fcntl(F_SETFL)`
  and from `ioctl(FIONBIO)`, and decides whether a receive passes timeout `0` or
  `u32::MAX`.
* **`poll`** collects the socket descriptors into one `NET_SOCKET_POLL` and
  sleeps inside it; files and the console answer immediately, as they always did.
* **`connect`** on a non-blocking socket passes timeout `0` and reports
  `EINPROGRESS`; the socket becomes writable when the handshake completes, which
  is exactly the contract a program expects, and `SO_ERROR` reports a refusal.
* **`getsockname`/`getpeername`** ask `NET_SOCKET_INFO`; the local address is
  the interface's.
* **Options** are accepted and have no effect (`setsockopt` returns 0), because
  the stack has nothing to tune and a program that asks is not asking for an
  error it can act on.

### Name resolution

musl's resolver is upstream code and it does its own DNS: it opens
`/etc/resolv.conf`, builds a query, and sends it over a UDP socket it creates
itself. Nothing in liblx resolves names; it makes the three things musl needs
exist:

1. **`/etc/resolv.conf` and `/etc/hosts`** are synthesized from what the service
   reports (`NET_INFO`), because there is no `/etc` on a FAT disk. A program
   opening either path gets a read-only memory file.
2. **`bind` and `recvmsg`** work on an unconnected UDP socket, and `recvmsg`
   reports the datagram's *source*, not the socket's connected peer. musl drops
   any answer that did not come from a name server it sent to, so a `recvmsg`
   that leaves `msg_name` untouched makes every lookup time out.
3. **`SOCK_NONBLOCK`** must survive `socket()`: musl's resolver sets it and then
   drives its own timeouts, and a blocking socket there hangs the program.

`getaddrinfo` and `gethostbyname_r` both end up on that path. curl's build -- IPv4
only -- uses `gethostbyname_r`.

## 5. curl and kirc

`scripts/build-curl.sh` builds curl 8.11.1 from `curl.se`, pinned by SHA-256, with
`scripts/lx-cc` as the cross compiler:

```sh
CC=$root/scripts/lx-cc ./configure --host=x86_64-nodeos-linux-gnu \
    --disable-shared --enable-static --without-ssl --disable-ipv6 \
    --disable-threaded-resolver --disable-ftp --disable-file ...
```

`--host` is what makes this a cross build: configure still compiles its probes
(they must link against musl and liblx to pass) but never runs them, so it never
starts a Linux binary on the host. Things it cannot check from a compile get the
conservative answer.

The result is an ordinary static Linux ELF, which `nxfpack` packs as
`/bin/curl.nxf` with the session's capabilities. `make` puts it on the disk once
it exists; it is not part of `make lx-apps` because it is a large download and a
long build.

What works today: `http://` URLs, IPv4 literals and names (which go through
musl's resolver), `-o` to a file, `-i`, `-v`, `-d`, `-H`, redirects and chunked
transfer encoding. What does not: `https://` (no TLS library is linked), IPv6,
HTTP/2, `--parallel` (the service is single-threaded and one process still does
one transfer at a time), and any URL scheme that needs a listener.

`scripts/build-kirc.sh` builds kirc 1.2.2, a terminal IRC client, from the
GitHub release tarball, pinned by SHA-256. Its Makefile takes `$(CC)` from a
variable, so the source stays exactly upstream's and the script passes
`make CC=$root/scripts/lx-cc`.

kirc exercises a different part of the same path. Where curl makes one request
and exits, kirc holds a TCP connection open and polls it against its keyboard;
it also puts the console in raw mode (docs/MUSL.md, 4.2) and redraws its input
line with ANSI erase and cursor-forward sequences. `make test-kirc` runs it
against `scripts/lib/ircd.py` on the host: registration, a `/join #test`, a
channel message, and the server's reply, the names list and its echo all read
back off the socket. The screen is a scrolling transcript, because the console
has no cursor addressing beyond one row; the IRC protocol path itself is
complete. DCC file transfer is not exercised: the receiving side opens a
listening socket, and the stack still returns `EOPNOTSUPP` for `listen`.

## 6. Limits, and what would come next

* **No listening sockets.** `listen` and `accept` report `EOPNOTSUPP`: nothing
  can connect *to* a NodeOS process yet. Doing it properly needs a policy
  decision the capability set does not have -- the shell grants `NETWORK`, and
  `NETWORK_BIND` exists in the ABI but nothing asks for it -- plus a port table
  owned by the service.
* **No IPv6.** smoltcp supports it; the interface has no address, and QEMU's
  user-mode network offers none that routes.
* **No TLS.** curl's `--without-ssl` is the honest state: an HTTPS fetch needs a
  TLS library built against this musl and liblx (mbedTLS is the smallest fit),
  an entropy source good enough for keys (liblx's `getrandom` is a xorshift),
  and a CA bundle on the disk at the path curl is told to read.
* **No DHCP renewal**, and one lease per boot. A long session on a network with
  short leases would lose its address; keeping the DHCP socket and renewing is a
  small change once the socket table can spare the slot.
* **One connection at a time per process**, because NodeOS has no threads
  (docs/MUSL.md, K2). curl works; a program that wants parallel transfers does
  not.
* **UDP datagrams larger than the caller's buffer are dropped**, not truncated:
  `recvfrom` on a socket with a small buffer loses the packet. DNS answers fit
  in the 512 bytes musl asks for.

## 7. Testing

```sh
make test-net      # DHCP, TCP from the host, DNS for example.com
make test-curl     # curl: fetch, save, read back, 256 KiB byte for byte, then
                   # example.com over the internet
```

Both drive QEMU's user-mode network (`-netdev user` plus an `e1000`). In that
network `10.0.2.2` is the host's loopback, so the tests start a Python HTTP
server on the host and fetch from it, then resolve and fetch a real name to
prove the path out. The DNS half needs the host to have working DNS; the TCP half
does not need the internet at all.

`netprobe` is the same checks without musl in the way:

```text
/> netprobe                          GET / from 10.0.2.2:80
/> netprobe 93.184.215.14 80 /       GET / from any address
/> netprobe dns example.com          one A query, over UDP
```
