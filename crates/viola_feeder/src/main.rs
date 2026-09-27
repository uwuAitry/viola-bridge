// SPDX-License-Identifier: GPL-3.0-or-later
//! viola_feeder — live Windows audio into `orender`'s named pipe.
//!
//! It captures a WASAPI endpoint (or loopback of a playback endpoint), writes a
//! streaming 44-byte RIFF/WAVE header once per engine connection, then streams
//! f32 PCM. `\\.\pipe\orender.input` is where `orender` listens, and the engine
//! creates that pipe itself when it is missing, so this side simply connects.
//!
//! No SDK is involved anywhere (see `docs/cloud-boundary.md`).
//!
//! ```text
//! viola_feeder --list
//! viola_feeder                                  # loopback of the default output
//! viola_feeder --device "Voicemeeter Out B1"    # capture a virtual cable's output
//! viola_feeder --device "Voicemeeter Input" --loopback
//! ```

#[cfg(windows)]
mod capture;
#[cfg(windows)]
mod pipe;
#[cfg(windows)]
mod wav;

#[cfg(windows)]
use std::time::{Duration, Instant};

#[cfg(windows)]
const CHUNK_FRAMES: usize = 1024;

#[cfg(windows)]
const USAGE: &str = "\
viola_feeder — feed live Windows audio into orender's named pipe

USAGE:
    viola_feeder [OPTIONS]

OPTIONS:
    --list                     list capture and render endpoints, then exit
    --device <NAME>            endpoint to use; substring match, case-insensitive
                               (default: the default output device, via loopback)
    --loopback                 treat --device as a *playback* endpoint and capture
                               what is played to it
    --rate <HZ>                sample rate to request       [default: 48000]
    --channels <N>             channel count to request     [default: 2]
    --pipe <PATH>              destination                 [default: \\\\.\\pipe\\orender.input]
    --seconds <N>              stop after N seconds        [default: 0 = until Ctrl-C]
    --connect-timeout-ms <MS>  how long to wait for the pipe  [default: 10000]
    --stats                    print throughput every 100 chunks
    -h, --help                 print this help
";

#[cfg(windows)]
struct Args {
    list: bool,
    device: Option<String>,
    loopback: bool,
    rate: usize,
    channels: usize,
    pipe: String,
    seconds: u64,
    connect_timeout: Duration,
    stats: bool,
}

#[cfg(windows)]
fn parse_args() -> Result<Option<Args>, String> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut args = Args {
        list: false,
        device: None,
        loopback: false,
        rate: 48_000,
        channels: 2,
        pipe: r"\\.\pipe\orender.input".to_string(),
        seconds: 0,
        connect_timeout: Duration::from_millis(10_000),
        stats: false,
    };
    let mut index = 0;
    while index < argv.len() {
        let flag = argv[index].clone();
        // Every value-taking flag pulls the next argument.
        let mut value = |what: &str| -> Result<String, String> {
            index += 1;
            argv.get(index)
                .cloned()
                .ok_or_else(|| format!("{what} needs a value"))
        };
        match flag.as_str() {
            "-h" | "--help" => return Ok(None),
            "--list" => args.list = true,
            "--loopback" => args.loopback = true,
            "--stats" => args.stats = true,
            "--device" => args.device = Some(value("--device")?),
            "--pipe" => args.pipe = value("--pipe")?,
            "--rate" => {
                args.rate = value("--rate")?.parse().map_err(|_| "--rate must be a number".to_string())?
            }
            "--channels" => {
                args.channels = value("--channels")?
                    .parse()
                    .map_err(|_| "--channels must be a number".to_string())?
            }
            "--seconds" => {
                args.seconds = value("--seconds")?
                    .parse()
                    .map_err(|_| "--seconds must be a number".to_string())?
            }
            "--connect-timeout-ms" => {
                let ms: u64 = value("--connect-timeout-ms")?
                    .parse()
                    .map_err(|_| "--connect-timeout-ms must be a number".to_string())?;
                args.connect_timeout = Duration::from_millis(ms);
            }
            other => return Err(format!("unknown option {other:?}")),
        }
        index += 1;
    }
    if args.channels == 0 || args.rate == 0 {
        return Err("--channels and --rate must be positive".into());
    }
    Ok(Some(args))
}

#[cfg(windows)]
fn run() -> Result<(), String> {
    let Some(args) = parse_args()? else {
        print!("{USAGE}");
        return Ok(());
    };

    wasapi::initialize_mta().ok().map_err(|e| e.to_string())?;

    if args.list {
        println!("capture endpoints (use with --device NAME):");
        for (index, name) in capture::enumerate(&wasapi::Direction::Capture)? {
            println!("  [{index}] {name}");
        }
        println!("render endpoints (use with --device NAME --loopback):");
        for (index, name) in capture::enumerate(&wasapi::Direction::Render)? {
            println!("  [{index}] {name}");
        }
        return Ok(());
    }

    let (device, direction) = capture::pick(args.device.as_deref(), args.loopback)?;
    let mut capture = capture::Capture::open(&device, args.rate, args.channels)?;

    let header = wav::streaming_header(args.rate as u32, args.channels as u16, 32);
    let mut sink = pipe::PipeWriter::new(args.pipe.clone());
    println!(
        "capturing {direction:?} at {} ch / {} Hz -> {}",
        args.channels,
        args.rate,
        sink.path().display()
    );

    let started = Instant::now();
    let mut connected = false;
    let mut chunks: u64 = 0;
    loop {
        if !connected {
            sink.connect(args.connect_timeout)?;
            sink.write_all(&header)?;
            connected = true;
            println!(
                "connected after {:.1}s; stream is {} ch / {} Hz f32",
                started.elapsed().as_secs_f32(),
                args.channels,
                args.rate
            );
        }

        let chunk = capture.read_frames(CHUNK_FRAMES)?;
        match sink.write_all(&chunk) {
            Ok(()) => {
                chunks += 1;
                if args.stats && chunks % 100 == 0 {
                    println!(
                        "{chunks} chunks, {:.1}s of audio",
                        (chunks as usize * CHUNK_FRAMES) as f32 / args.rate as f32
                    );
                }
            }
            Err(err) => {
                eprintln!("viola_feeder: {err} — reconnecting");
                connected = false;
            }
        }

        if args.seconds > 0 && started.elapsed() >= Duration::from_secs(args.seconds) {
            break;
        }
    }

    println!(
        "done: {chunks} chunks ({:.1}s of audio) in {:.1}s",
        (chunks as usize * CHUNK_FRAMES) as f32 / args.rate as f32,
        started.elapsed().as_secs_f32()
    );
    Ok(())
}

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("viola_feeder: {message}");
            eprintln!("try --help");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("viola_feeder is Windows-only: it captures through WASAPI.");
    std::process::exit(2);
}
