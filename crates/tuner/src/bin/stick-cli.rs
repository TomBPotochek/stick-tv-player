//! Small command-line tool for testing the tuner without the GUI.
//!
//! stick-cli list
//! stick-cli scan [--from 14] [--to 51]
//! stick-cli status CH
//! stick-cli tune CH [-o FILE] [--seconds N] [--service SID]     (FILE "-" = stdout)
//! stick-cli demux FILE                                          (list the services in a recorded TS)
//! stick-cli filter IN SID OUT                                   (keep one service of a recorded TS, as the app does)

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use stick_tuner::channels::{self, uhf_center_khz};
use stick_tuner::ts::{Demux, Packetizer, ServiceFilter};
use stick_tuner::usb::{self, Stick};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let r = match args.first().map(String::as_str) {
        Some("list") => list(),
        Some("scan") => scan(&args[1..]),
        Some("status") => status(&args[1..]),
        Some("tune") => tune(&args[1..]),
        Some("demux") => demux_file(&args[1..]),
        Some("reset") => Stick::open(None, 0)
            .and_then(|s| s.reset_device())
            .map_err(|e| e.into()),
        Some("filter") => filter_file(&args[1..]),
        _ => {
            eprintln!("usage: stick-cli list | scan [--from 14] [--to 51] | status CH | tune CH [-o FILE] [--seconds N] [--service SID] | demux FILE | filter IN SID OUT  [-v]");
            std::process::exit(2);
        }
    };
    if let Err(e) = r {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

type R = Result<(), Box<dyn std::error::Error>>;

fn verbose() -> bool {
    std::env::args().any(|a| a == "-v")
}

fn flush_log(s: &Stick) {
    let l = s.drain_log();
    if verbose() && !l.trim().is_empty() {
        eprintln!("{}", l.trim_end());
    }
}

fn open() -> Result<Stick, Box<dyn std::error::Error>> {
    let s = Stick::open(None, 0)?;
    let t0 = Instant::now();
    let r = s.init();
    flush_log(&s);
    if let Err(e) = r {
        eprintln!("{}", s.drain_log());
        return Err(e.into());
    }
    eprintln!(
        "{} ready, firmware {} ({} ms)",
        s.profile_name(),
        s.firmware_version_string().unwrap_or_default(),
        t0.elapsed().as_millis()
    );
    Ok(s)
}

fn arg<T: std::str::FromStr>(args: &[String], name: &str) -> Option<T> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
}

fn parse_ch(s: &str) -> Result<u32, Box<dyn std::error::Error>> {
    let n: u32 = s.trim_start_matches("ch").parse()?;
    Ok(if (14..=69).contains(&n) {
        uhf_center_khz(n as u8)
    } else {
        n
    })
}

fn demux_file(args: &[String]) -> R {
    let data = std::fs::read(args.first().ok_or("demux needs a file")?)?;
    let mut d = Demux::new();
    let mut pk = Packetizer::default();
    let mut at = None;
    for (i, chunk) in data.chunks(65536).enumerate() {
        pk.feed(chunk, |p| d.packet(p));
        if at.is_none() && d.has_basic_info() {
            at = Some(i * 65536);
        }
    }
    println!(
        "packets {} tei {} cc_errors {} sync_losses {} | pat {} sdt {} nit {} (basic info after {:?} bytes)",
        d.packets, d.tei_packets, d.cc_errors, pk.sync_losses, d.pat_seen, d.sdt_seen, d.nit_seen, at
    );
    println!("network '{}' ts_id {:?}", d.network_name, d.ts_id);
    for s in d.services() {
        println!(
            "  {:6} sid 0x{:04X} pmt {:?} type {:?} one_seg {} pmt_seen {} '{}' streams {:?}",
            s.virtual_number,
            s.service_id,
            s.pmt_pid,
            s.service_type,
            s.one_seg,
            s.pmt_seen,
            s.name,
            s.streams
                .iter()
                .map(|x| format!("{:#x}:{}", x.pid, x.codec()))
                .collect::<Vec<_>>()
        );
    }
    Ok(())
}

fn filter_file(args: &[String]) -> R {
    let [input, sid, output] = args else {
        return Err("filter needs IN SID OUT".into());
    };
    let sid = u16::from_str_radix(sid.trim_start_matches("0x"), 16)?;
    let data = std::fs::read(input)?;
    let (mut d, mut f, mut pk, mut out) = (
        Demux::new(),
        ServiceFilter::new(sid),
        Packetizer::default(),
        vec![],
    );
    pk.feed(&data, |p| {
        d.packet(p);
        f.update(&d);
        f.packet(p, &mut out);
    });
    std::fs::write(output, &out)?;
    println!("{} -> {} bytes", data.len(), out.len());
    Ok(())
}

fn list() -> R {
    let l = usb::list()?;
    if l.is_empty() {
        println!("no supported stick connected");
    }
    for d in l {
        println!("{d}");
    }
    Ok(())
}

fn scan(args: &[String]) -> R {
    let from = arg(args, "--from").unwrap_or(channels::UHF_FIRST);
    let to = arg(args, "--to").unwrap_or(channels::UHF_LAST);
    let s = open()?;
    let cancel = AtomicBool::new(false);
    let list = channels::scan(&s, from, to, &cancel, |p| {
        eprint!(
            "\rch {:2} {:.3} MHz {:8}  ",
            p.physical,
            p.freq_khz as f64 / 1000.0,
            p.stage
        );
        flush_log(&s);
    })?;
    eprintln!();
    for c in &list {
        println!(
            "ch {:2}  {:6} sid 0x{:04X}  {:8}  {:<24} {}  [{}/{}]",
            c.physical,
            c.virtual_number,
            c.service_id,
            if c.one_seg { "one-seg" } else { "full-seg" },
            c.name,
            c.network_name,
            c.video.as_deref().unwrap_or("-"),
            c.audio.as_deref().unwrap_or("-")
        );
    }
    Ok(())
}

fn status(args: &[String]) -> R {
    let khz = parse_ch(args.first().ok_or("status needs a channel")?)?;
    let s = open()?;
    let lk = s.tune(khz, false)?;
    flush_log(&s);
    println!(
        "{} {:.3} MHz",
        if lk { "LOCKED" } else { "NO LOCK" },
        khz as f64 / 1000.0
    );
    println!("{:?}", s.status()?);
    Ok(())
}

fn tune(args: &[String]) -> R {
    let khz = parse_ch(args.first().ok_or("tune needs a channel")?)?;
    let out_path: String = arg(args, "-o").unwrap_or_else(|| "-".into());
    let seconds: Option<f64> = arg(args, "--seconds");
    let service: Option<u16> = args
        .iter()
        .position(|a| a == "--service")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| u16::from_str_radix(v.trim_start_matches("0x"), 16).ok());
    let s = Arc::new(open()?);
    let lk = s.tune(khz, false)?;
    flush_log(&s);
    eprintln!(
        "{} {:.3} MHz",
        if lk { "LOCKED" } else { "NO LOCK" },
        khz as f64 / 1000.0
    );
    let mut out: Box<dyn Write + Send> = if out_path == "-" {
        Box::new(std::io::BufWriter::with_capacity(
            1 << 20,
            std::io::stdout(),
        ))
    } else {
        Box::new(std::io::BufWriter::with_capacity(
            1 << 20,
            std::fs::File::create(&out_path)?,
        ))
    };
    let stop = Arc::new(AtomicBool::new(false));
    let deadline = seconds.map(|x| Instant::now() + Duration::from_secs_f64(x));
    {
        let (s2, stop2) = (s.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut last = (0u64, Instant::now());
            while !stop2.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(2));
                let b = s2.ts_bytes();
                let mbit = (b - last.0) as f64 * 8.0 / 1e6 / last.1.elapsed().as_secs_f64();
                last = (b, Instant::now());
                let sig = s2.status().map(|x| {
                    format!(
                        "{:?}%/{:?}% lock={}",
                        x.strength_percent, x.quality_percent, x.locked
                    )
                });
                eprint!(
                    "\r{}  {mbit:.2} Mbit/s  {} MB  dropped {}   ",
                    sig.unwrap_or_default(),
                    b >> 20,
                    s2.dropped_chunks()
                );
            }
        });
    }
    let mut pk = Packetizer::default();
    let mut demux = Demux::new();
    let mut filter = service.map(ServiceFilter::new);
    let mut buf = Vec::with_capacity(1 << 16);
    let r = s.stream(
        |chunk| match filter.as_mut() {
            None => out.write_all(chunk).is_ok(),
            Some(f) => {
                buf.clear();
                pk.feed(chunk, |p| {
                    demux.packet(p);
                    f.update(&demux);
                    f.packet(p, &mut buf);
                });
                out.write_all(&buf).is_ok()
            }
        },
        || !stop.load(Ordering::Relaxed) && deadline.is_none_or(|d| Instant::now() < d),
    );
    stop.store(true, Ordering::Relaxed);
    eprintln!(
        "\nstopped: {} bytes of TS, {} chunks dropped",
        s.ts_bytes(),
        s.dropped_chunks()
    );
    flush_log(&s);
    r?;
    Ok(())
}
