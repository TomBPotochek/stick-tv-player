//! Differential test of the Rust IT9175 port against traces of the real vendor libraries (libpt275aDrv.so and
//! libPTx1Drv.so run under a CPU emulator against a fake device, see OpenDTV tools/emu). Port of OpenDTV's
//! `TunerDiffTest`: for every scenario both sides get the same register read values and must produce the identical
//! sequence of register writes.

use std::collections::{HashMap, HashSet};
use stick_tuner::it9175::{data_pt275::PT275, data_pt275c::PT275C, profile::Profile, Io, It9175};

fn key(p: u8, a: u32) -> u64 {
    ((p as u64) << 32) | a as u64
}

fn pseudo(p: u8, a: u32) -> u8 {
    ((a as u64 * 131 + p as u64 * 17 + 7) & 0xff) as u8
}

struct FakeIo {
    expl: HashMap<u64, u8>,
    targets: HashSet<u64>,
    ops: Vec<String>,
}

impl Io for FakeIo {
    fn read(&mut self, proc: u8, addr: u32, n: usize) -> Option<Vec<u8>> {
        Some(
            (0..n as u32)
                .map(|i| {
                    let k = key(proc, addr + i);
                    match self.expl.get(&k) {
                        Some(v) => *v,
                        None if self.targets.contains(&k) => pseudo(proc, addr + i),
                        None => 0,
                    }
                })
                .collect(),
        )
    }

    fn write(&mut self, proc: u8, addr: u32, data: &[u8]) -> bool {
        let hex: String = data.iter().map(|b| format!("{b:02x}")).collect();
        self.ops.push(format!("W {proc} {addr:x} {hex}"));
        true
    }

    fn fw_version(&mut self, proc: u8) -> bool {
        self.ops.push(format!("C {proc} 22"));
        true
    }

    fn sleep(&mut self, _ms: u32) {}
}

fn run(golden: &str, profile: &'static Profile) {
    let text = std::fs::read_to_string(golden).unwrap_or_else(|e| panic!("{golden}: {e}"));
    let mut targets = HashSet::new();
    let mut sc: Vec<String> = vec![];
    let mut exp: Vec<String> = vec![];
    let (mut scenarios, mut failed) = (0, 0);
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("T ") {
            let p: Vec<&str> = rest.split(' ').collect();
            targets.insert(key(
                p[0].parse().unwrap(),
                u32::from_str_radix(p[1], 16).unwrap(),
            ));
        } else if let Some(rest) = line.strip_prefix("S ") {
            sc = rest.split(' ').map(String::from).collect();
            exp.clear();
        } else if let Some(rest) = line.strip_prefix("O ") {
            exp.push(rest.to_string());
        } else if line == "E" {
            scenarios += 1;
            let n: Vec<u32> = sc.iter().map(|s| s.parse().unwrap()).collect();
            let (clk, d1, ec86, ed03, ed23, ed81, ec4c, r4d, freq) =
                (n[0], n[1], n[2], n[3], n[4], n[5], n[6], n[7], n[8]);
            let mut t2 = targets.clone();
            for k in [
                key(0, 0xd800),
                key(8, 0x1dc),
                key(8, 0xec86),
                key(8, 0xec4c),
                key(8, 0xed81),
                key(8, 0x4d),
                key(8, 0x1c6),
            ] {
                t2.remove(&k);
            }
            let mut expl = HashMap::new();
            expl.insert(key(0, 0xd800), clk as u8);
            expl.insert(key(0, 0x1222), 1);
            expl.insert(key(0, 0x1223), 0x75);
            expl.insert(key(0, 0x1224), 0x91);
            expl.insert(key(8, 0xec86), ec86 as u8);
            expl.insert(key(8, 0xed03), ed03 as u8);
            expl.insert(key(8, 0xed23), (ed23 & 0xff) as u8);
            expl.insert(key(8, 0xed24), (ed23 >> 8) as u8);
            expl.insert(key(8, 0xed81), ed81 as u8);
            expl.insert(key(8, 0xec4c), ec4c as u8);
            expl.insert(key(8, 0x1dc), d1 as u8);
            expl.insert(key(8, 0x1c6), 1);
            expl.insert(key(8, 0x4d), r4d as u8);
            let mut io = FakeIo {
                expl,
                targets: t2,
                ops: vec![],
            };
            let mut t = It9175::new(profile);
            let ok = t.init(&mut io) && t.tune(&mut io, freq);
            if !ok || io.ops != exp {
                failed += 1;
                let i = exp
                    .iter()
                    .zip(io.ops.iter())
                    .take_while(|(a, b)| a == b)
                    .count();
                eprintln!("FAIL scenario {scenarios} {} ok={ok}", sc.join(" "));
                eprintln!(
                    "  first diff at op {i} (exp {}, got {})",
                    exp.len(),
                    io.ops.len()
                );
                if let Some(e) = exp.get(i) {
                    eprintln!("  exp: {e}");
                }
                if let Some(g) = io.ops.get(i) {
                    eprintln!("  got: {g}");
                }
                eprintln!("  log: {}", t.log.trim());
                if failed > 5 {
                    break;
                }
            }
        }
    }
    println!("{}: {scenarios} scenarios, {failed} failed", profile.name);
    assert!(scenarios > 0, "no scenarios in {golden}");
    assert_eq!(
        failed, 0,
        "{failed} of {scenarios} scenarios differ from the vendor trace"
    );
}

#[test]
fn pt275_matches_libpt275a_golden() {
    run(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/tuner_golden.txt"),
        &PT275,
    );
}

#[test]
fn pt275c_matches_libptx1_golden() {
    run(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/golden/tuner_golden_ptx1.txt"
        ),
        &PT275C,
    );
}
