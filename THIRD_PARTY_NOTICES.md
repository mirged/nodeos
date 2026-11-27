# Third-party notices

NodeOS is released under the [MIT License](LICENSE). That license covers the
kernel, user programs, tools, scripts and documentation in this repository,
except for the third-party sources listed here, which stay under their own
licenses. Vendored files keep their original copyright and license notices;
do not strip them.

## Doom port

`user/doom/csrc/` is [doomgeneric](https://github.com/ozkl/doomgeneric), a
portable fork of Chocolate Doom, under the
[GNU General Public License, version 2 or later](user/doom/csrc/COPYING).
Copyright (C) 1993-1996 Id Software, Inc., (C) 2005-2014 Simon Howard and
other contributors, as stated in each file.

The NodeOS platform files in that directory (`doomgeneric_nodeos.c`,
`i_nodeossound.c`) are distributed under the same license, and so is the
`doom` program built from them. The rest of `user/doom/` (the Rust launcher in
`src/`, the freestanding libc in `include/` and `libc/`, and the tests) is
MIT-licensed with the rest of NodeOS.

The Doom IWAD (`DOOM.WAD`) is not part of this repository and is not covered by
any license here. Use a shareware `doom1.wad` or a copy of the game you own.

## musl

The files under `user/lx/musl/` replace parts of
[musl](https://musl.libc.org/) 1.2.6 and are derived from it. musl is under the
MIT license, copyright (c) 2005-2020 Rich Felker, et al. See musl's
`COPYRIGHT` file for the full notice.

## Terminal font

`user/libdraw/src/terminal-font/` contains glyph atlases derived from DejaVu
Sans Mono, under the Bitstream Vera license with DejaVu changes in the public
domain. The full text is in
[`LICENSE.txt`](user/libdraw/src/terminal-font/LICENSE.txt).

## Built from upstream sources

These programs are not vendored. Build scripts download them and the disk image
includes the result; each keeps its own license:

- [musl](https://musl.libc.org/) 1.2.6 (`scripts/build-musl.sh`), MIT
- [curl](https://curl.se/) 8.11.1 (`scripts/build-curl.sh`), the curl license
  (MIT-style)
- [kirc](https://github.com/mcpcpc/kirc) 1.2.2 (`scripts/build-kirc.sh`), MIT
- [Lua](https://www.lua.org/) 5.4.7 (`scripts/build-lx-apps.sh`), MIT
