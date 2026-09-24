//! Compiles the C half of the port -- Doom itself, and the mini-libc under
//! `libc/` -- into a static archive for the Rust binary to link.
//!
//! # Why gcc, and why these flags
//!
//! There is no cross-compiler in this tree and none is needed: the host is
//! x86-64, so the host gcc produces exactly the right code once told to stop
//! assuming an operating system. The flags fall into three groups.
//!
//! *Freestanding.* `-ffreestanding -nostdinc` cut the C library loose; the
//! only headers on the search path are `include/` (ours) and gcc's own
//! freestanding set, which supplies `stdint.h`, `stddef.h` and `stdarg.h`.
//!
//! *Matching the Rust half.* `-mno-red-zone`, `-fno-pic` and
//! `-mcmodel=small` mirror the rustflags in `user/.cargo/config.toml`. The red
//! zone is the load-bearing one: NodeOS lands interrupts on the kernel stack
//! of the interrupted thread, and a leaf function that keeps live data below
//! its own stack pointer would have it overwritten.
//!
//! *No floating point.* `-mgeneral-regs-only` forbids SSE and x87 outright.
//! This is not an optimisation. NodeOS's context switch saves general-purpose
//! registers and nothing else -- there is no FXSAVE anywhere in the kernel --
//! so a userspace program with live floating-point state across a preemption
//! would be corrupted by whatever ran next. Doom is fixed-point throughout,
//! which makes obeying that restriction free, and the compiler now *enforces*
//! it: a float that crept in would fail to compile rather than fail at 3am in
//! a way nobody could reproduce. (See `docs/DOOM.md`.)

use std::path::{Path, PathBuf};
use std::process::Command;

/// The Doom translation units, from doomgeneric's own Makefile, minus the
/// platform backends (SDL, X11, Windows, Allegro, ...) and plus ours.
const DOOM_SOURCES: &[&str] = &[
    "am_map", "doomdef", "doomstat", "dstrings", "d_event", "d_items", "d_iwad",
    "d_loop", "d_main", "d_mode", "d_net", "f_finale", "f_wipe", "g_game",
    "hu_lib", "hu_stuff", "info", "i_cdmus", "i_endoom", "i_joystick",
    "i_scale", "i_sound", "i_system", "i_timer", "memio", "m_argv", "m_bbox",
    "m_cheat", "m_config", "m_controls", "m_fixed", "m_menu", "m_misc",
    "m_random", "p_ceilng", "p_doors", "p_enemy", "p_floor", "p_inter",
    "p_lights", "p_map", "p_maputl", "p_mobj", "p_plats", "p_pspr", "p_saveg",
    "p_setup", "p_sight", "p_spec", "p_switch", "p_telept", "p_tick", "p_user",
    "r_bsp", "r_data", "r_draw", "r_main", "r_plane", "r_segs", "r_sky",
    "r_things", "sha1", "sounds", "statdump", "st_lib", "st_stuff", "s_sound",
    "tables", "v_video", "wi_stuff", "w_checksum", "w_file", "w_main", "w_wad",
    "z_zone", "w_file_stdc", "i_input", "i_video", "doomgeneric", "dummy",
    "doomgeneric_nodeos", "i_nodeossound",
];

/// The mini-libc.
const LIBC_SOURCES: &[&str] = &["string", "ctype", "stdlib", "printf", "stdio", "malloc"];

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
    let cc = std::env::var("CC").unwrap_or_else(|_| "gcc".into());

    // gcc's freestanding headers (stdint.h, stddef.h, stdarg.h). Asking the
    // compiler where they are beats hard-coding a version number.
    let gcc_include = run(&cc, &["-print-file-name=include"]);
    let gcc_include = gcc_include.trim();

    let mut objects = Vec::new();

    for (dir, sources) in [("csrc", DOOM_SOURCES), ("libc", LIBC_SOURCES)] {
        for name in sources {
            let source = root.join(dir).join(format!("{name}.c"));
            let object = out.join(format!("{dir}_{name}.o"));
            compile(&cc, &source, &object, gcc_include, &root);
            println!("cargo:rerun-if-changed={}", source.display());
            objects.push(object);
        }
    }

    for header in ["csrc", "libc", "include"] {
        println!("cargo:rerun-if-changed={}", root.join(header).display());
    }

    // Archive, then hand the linker the search path and the name.
    let archive = out.join("libdoomc.a");
    let _ = std::fs::remove_file(&archive);
    let ar = std::env::var("AR").unwrap_or_else(|_| "ar".into());
    let mut args: Vec<String> = vec!["rcs".into(), archive.display().to_string()];
    args.extend(objects.iter().map(|o| o.display().to_string()));
    run(&ar, &args.iter().map(String::as_str).collect::<Vec<_>>());

    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=doomc");
    println!("cargo:rerun-if-changed=build.rs");
}

fn compile(cc: &str, source: &Path, object: &Path, gcc_include: &str, root: &Path) {
    let include = root.join("include");
    let csrc = root.join("csrc");

    run(
        cc,
        &[
            "-c",
            &source.display().to_string(),
            "-o",
            &object.display().to_string(),
            "-O2",
            "-std=gnu99",
            // Freestanding: our headers first, then gcc's own.
            "-ffreestanding",
            "-nostdinc",
            "-isystem",
            gcc_include,
            "-I",
            &include.display().to_string(),
            "-I",
            &csrc.display().to_string(),
            // Match the Rust half's code generation (user/.cargo/config.toml).
            "-mno-red-zone",
            "-mcmodel=small",
            "-fno-pic",
            "-fno-stack-protector",
            "-fno-asynchronous-unwind-tables",
            "-fno-unwind-tables",
            // No SSE, no x87: see the module comment.
            "-mgeneral-regs-only",
            // Doom renders at its original 320x200 and this port scales the
            // result to whatever the boot framebuffer turned out to be, so
            // DG_ScreenBuffer stays one frame's worth of pixels.
            "-DDOOMGENERIC_RESX=320",
            "-DDOOMGENERIC_RESY=200",
            "-DNORMALUNIX",
            // Marks this port's divergences inside the Doom sources.
            "-D__NODEOS__",
            // Turns on csrc/i_nodeossound.c and its registration in i_sound.c.
            // Deliberately not FEATURE_SOUND -- see the comment there.
            "-D__NODEOS_SOUND__",
            // Doom's own warnings are not this port's business to fix; the
            // ones that would indicate a porting mistake are kept.
            "-Wall",
            "-Wno-unused-variable",
            "-Wno-unused-but-set-variable",
            "-Wno-unused-function",
            "-Wno-parentheses",
            "-Wno-format-truncation",
            "-Wno-stringop-truncation",
            "-Wno-sign-compare",
            "-Wno-misleading-indentation",
            "-Wno-implicit-fallthrough",
            "-Wno-array-bounds",
        ],
    );
}

fn run(program: &str, args: &[&str]) -> String {
    let output = Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("failed to run {program}: {error}"));

    if !output.status.success() {
        panic!(
            "{program} {}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    String::from_utf8_lossy(&output.stdout).into_owned()
}
