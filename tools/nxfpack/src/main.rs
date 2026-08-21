//! `nxfpack` -- turn a linked ELF into a NodeOS NXF image.
//!
//! The compiler and linker still speak ELF, because rewriting a code generator
//! to emit a new container would be a poor use of anyone's time. NXF is what
//! the *operating system* consumes, so the translation happens here, once, at
//! packaging time -- which is also the natural place to attach the things ELF
//! has no vocabulary for: the capability requirements and the resource
//! envelope.
//!
//! ```text
//! nxfpack target/user/release/init -o build/init.nxf \
//!         --name init --service \
//!         --cap PROCESS --cap CHANNEL --cap FILE_READ:optional \
//!         --stack 262144 --heap 2097152
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;

use nodeos_abi::nxf;

mod elf;
mod writer;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("nxfpack: {message}");
            ExitCode::FAILURE
        }
    }
}

struct Options {
    input: PathBuf,
    output: PathBuf,
    name: String,
    version: u32,
    capabilities: Vec<(String, bool, Option<String>)>,
    dependencies: Vec<(String, u32)>,
    resources: nxf::ResourceEnvelope,
    flags: u32,
    verbose: bool,
}

fn run() -> Result<(), String> {
    let options = parse_args()?;
    let bytes = std::fs::read(&options.input).map_err(|e| format!("cannot read {}: {e}", options.input.display()))?;
    let elf = elf::Elf::parse(&bytes)?;

    if options.verbose {
        eprintln!("nxfpack: {} -> {}", options.input.display(), options.output.display());
        eprintln!("  entry {:#x}, {} loadable segments", elf.entry, elf.segments.len());
    }

    let image = writer::build(&elf, &options)?;
    std::fs::write(&options.output, &image).map_err(|e| format!("cannot write {}: {e}", options.output.display()))?;

    // Round-trip: parse what we just wrote with the very code the kernel uses.
    // A packer that emits an image the loader rejects is worse than one that
    // fails, because the failure surfaces at boot instead of at build time.
    let parsed = nxf::Image::parse(&image).map_err(|e| format!("self-check failed to parse output: {e:?}"))?;
    parsed.verify_checksum().map_err(|e| format!("self-check checksum: {e:?}"))?;

    println!(
        "nxfpack: wrote {} ({} bytes, entry {:#x}, image {} KiB, {} segments, {} capabilities)",
        options.output.display(),
        image.len(),
        parsed.header().entry,
        parsed.header().image_size / 1024,
        parsed.header().segment_count,
        parsed.header().cap_count
    );
    Ok(())
}

fn parse_args() -> Result<Options, String> {
    let mut args = std::env::args().skip(1);
    let mut input: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    let mut name: Option<String> = None;
    let mut version = 1u32;
    let mut capabilities = Vec::new();
    let mut dependencies = Vec::new();
    let mut resources = nxf::ResourceEnvelope::default();
    let mut flags = 0u32;
    let mut verbose = false;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-o" | "--output" => output = Some(PathBuf::from(next(&mut args, "--output")?)),
            "--name" => name = Some(next(&mut args, "--name")?),
            "--version" => version = parse_number(&next(&mut args, "--version")?)? as u32,
            "--cap" => {
                let spec = next(&mut args, "--cap")?;
                let mut parts = spec.splitn(3, ':');
                let class = parts.next().unwrap_or("").to_string();
                let mut optional = false;
                let mut detail = None;
                for part in parts {
                    match part {
                        "optional" => optional = true,
                        "required" => optional = false,
                        other => detail = Some(other.to_string()),
                    }
                }
                capabilities.push((class, optional, detail));
            }
            "--dep" => {
                let spec = next(&mut args, "--dep")?;
                let (dep_name, dep_version) = match spec.split_once('@') {
                    Some((n, v)) => (n.to_string(), parse_number(v)? as u32),
                    None => (spec, 0),
                };
                dependencies.push((dep_name, dep_version));
            }
            "--stack" => resources.stack_size = parse_number(&next(&mut args, "--stack")?)?,
            "--heap" => resources.heap_reserve = parse_number(&next(&mut args, "--heap")?)?,
            "--max-memory" => resources.max_memory_bytes = parse_number(&next(&mut args, "--max-memory")?)?,
            "--max-threads" => resources.max_threads = parse_number(&next(&mut args, "--max-threads")?)? as u32,
            "--max-handles" => resources.max_handles = parse_number(&next(&mut args, "--max-handles")?)? as u32,
            "--sched" => {
                let class = next(&mut args, "--sched")?;
                resources.sched_class = match class.as_str() {
                    "realtime" => 0,
                    "interactive" => 1,
                    "normal" => 2,
                    "background" => 3,
                    other => return Err(format!("unknown scheduling class {other:?}")),
                };
            }
            "--priority" => resources.sched_priority = parse_number(&next(&mut args, "--priority")?)? as u32,
            "--service" => flags |= nxf::image_flags::SERVICE,
            "--driver" => flags |= nxf::image_flags::DRIVER,
            "--strict" => flags |= nxf::image_flags::STRICT_CAPABILITIES,
            "--pure" => flags |= nxf::image_flags::PURE,
            "-v" | "--verbose" => verbose = true,
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            other if other.starts_with('-') => return Err(format!("unknown option {other:?}")),
            other => input = Some(PathBuf::from(other)),
        }
    }

    let input = input.ok_or("no input file")?;
    let output = output.unwrap_or_else(|| input.with_extension("nxf"));
    let name = name.unwrap_or_else(|| {
        input.file_stem().and_then(|s| s.to_str()).unwrap_or("unnamed").to_string()
    });

    Ok(Options { input, output, name, version, capabilities, dependencies, resources, flags, verbose })
}

fn next(args: &mut impl Iterator<Item = String>, option: &str) -> Result<String, String> {
    args.next().ok_or_else(|| format!("{option} needs a value"))
}

fn parse_number(text: &str) -> Result<u64, String> {
    let text = text.replace('_', "");
    let (digits, multiplier) = match text.chars().last() {
        Some('K') | Some('k') => (&text[..text.len() - 1], 1024),
        Some('M') | Some('m') => (&text[..text.len() - 1], 1024 * 1024),
        Some('G') | Some('g') => (&text[..text.len() - 1], 1024 * 1024 * 1024),
        _ => (text.as_str(), 1),
    };
    let value = if let Some(hex) = digits.strip_prefix("0x") {
        u64::from_str_radix(hex, 16)
    } else {
        digits.parse::<u64>()
    };
    value.map(|v| v * multiplier).map_err(|_| format!("not a number: {text:?}"))
}

/// Map a capability class name to its ABI discriminant.
pub fn capability_class(name: &str) -> Option<u32> {
    let table: BTreeMap<&str, u32> = BTreeMap::from([
        ("FILE_READ", 1),
        ("FILE_WRITE", 2),
        ("FILE_CREATE", 3),
        ("NETWORK", 4),
        ("NETWORK_BIND", 5),
        ("GPU", 6),
        ("AUDIO", 7),
        ("MEMORY", 8),
        ("PROCESS", 9),
        ("CHANNEL", 10),
        ("DEVICE", 11),
        ("CLOCK", 12),
        ("DEBUG", 13),
        ("LOAD", 14),
        ("INPUT", 15),
        ("DISPLAY", 16),
    ]);
    table.get(name).copied()
}

fn print_usage() {
    println!(
        "\
nxfpack -- package a linked ELF as a NodeOS NXF image

USAGE:
    nxfpack <input.elf> [-o <output.nxf>] [options]

OPTIONS:
    --name <name>            image name (default: input file stem)
    --version <n>            image version for dependency resolution
    --cap <CLASS[:optional][:detail]>
                             declare a required capability class; repeatable
    --dep <name[@version]>   declare a dependency; repeatable
    --stack <bytes>          initial stack size
    --heap <bytes>           heap to reserve at startup
    --max-memory <bytes>     resource envelope: total mapped memory
    --max-threads <n>        resource envelope: thread count
    --max-handles <n>        resource envelope: handle count
    --sched <class>          realtime | interactive | normal | background
    --priority <n>           priority within the class, 0..31
    --service                mark as a long-lived system service
    --driver                 mark as a device driver
    --strict                 refuse to start unless every capability is granted
    --pure                   the image needs no capabilities at all
    -v, --verbose            explain what is being packed

Sizes accept K, M and G suffixes and 0x hex."
    );
}
