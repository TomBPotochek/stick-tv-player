//! Demux + service filter on a synthetic ISDB-Tb multiplex (PAT, two PMTs, SDT, NIT with partial reception).

use stick_tuner::ts::{crc32_mpeg, pid, Demux, Packetizer, ServiceFilter, PKT};

fn section(table_id: u8, ext: u16, body: &[u8]) -> Vec<u8> {
    // long-form header: table_id, length, ext, version/current, section numbers
    let len = 5 + body.len() + 4;
    let mut s = vec![table_id, 0xB0 | ((len >> 8) as u8 & 0x0F), len as u8];
    s.extend_from_slice(&ext.to_be_bytes());
    s.extend_from_slice(&[0xC1, 0, 0]);
    s.extend_from_slice(body);
    let c = crc32_mpeg(&s);
    s.extend_from_slice(&c.to_be_bytes());
    s
}

/// Packs a section into TS packets on `pid` (pointer field 0, 0xFF stuffing).
fn packets(pid: u16, s: &[u8], cc: &mut u8) -> Vec<u8> {
    let mut out = vec![];
    let mut first = true;
    let mut rest: &[u8] = s;
    while first || !rest.is_empty() {
        let mut p = vec![0xFFu8; PKT];
        p[0] = 0x47;
        p[1] = (if first { 0x40 } else { 0 }) | (pid >> 8) as u8;
        p[2] = pid as u8;
        p[3] = 0x10 | (*cc & 0x0F);
        *cc = cc.wrapping_add(1);
        let mut o = 4;
        if first {
            p[4] = 0;
            o = 5;
        }
        let n = rest.len().min(PKT - o);
        p[o..o + n].copy_from_slice(&rest[..n]);
        rest = &rest[n..];
        first = false;
        out.extend_from_slice(&p);
    }
    out
}

fn es_packet(pid: u16) -> Vec<u8> {
    let mut p = vec![0u8; PKT];
    p[0] = 0x47;
    // each packet starts a PES (00 00 01 E0, unbounded length, no optional fields)
    p[1] = 0x40 | (pid >> 8) as u8;
    p[2] = pid as u8;
    p[3] = 0x10;
    p[4..13].copy_from_slice(&[0, 0, 1, 0xE0, 0, 0, 0x80, 0, 0]);
    p
}

fn mux() -> Vec<u8> {
    let mut ts = vec![];
    let mut cc = 0u8;
    // PAT: ts_id 0x0640; NIT on 0x10; svc 0xE760 -> PMT 0x1F0; one-seg svc 0xE778 -> PMT 0x1FC8
    let pat = section(
        0x00,
        0x0640,
        &[
            0, 0, 0xE0, 0x10, 0xE7, 0x60, 0xE1, 0xF0, 0xE7, 0x78, 0xFF, 0xC8,
        ],
    );
    ts.extend(packets(0, &pat, &mut cc));
    // PMT full-seg: PCR 0x111, H.264 0x111, AAC-LATM 0x112, data 0x113
    let pmt1 = section(
        0x02,
        0xE760,
        &[
            0xE1, 0x11, 0xF0, 0x00, 0x1B, 0xE1, 0x11, 0xF0, 0x00, 0x11, 0xE1, 0x12, 0xF0, 0x00,
            0x0D, 0xE1, 0x13, 0xF0, 0x00,
        ],
    );
    ts.extend(packets(0x1F0, &pmt1, &mut cc));
    let pmt2 = section(
        0x02,
        0xE778,
        &[
            0xE8, 0x11, 0xF0, 0x00, 0x1B, 0xE8, 0x11, 0xF0, 0x00, 0x11, 0xE8, 0x12, 0xF0, 0x00,
        ],
    );
    ts.extend(packets(0x1FC8, &pmt2, &mut cc));
    // SDT: original_network_id + reserved, then two services with service descriptors
    let mut sdt = vec![0x06, 0x40, 0xFF];
    for (sid, ty, name) in [
        (0xE760u16, 0x01u8, &b"Am\xe9rica HD"[..]),
        (0xE778, 0xC0, &b"Am\xe9rica 1seg"[..]),
    ] {
        let mut desc = vec![0x48, 0, ty, 0];
        desc.push(name.len() as u8);
        desc.extend_from_slice(name);
        desc[1] = (desc.len() - 2) as u8;
        sdt.extend_from_slice(&sid.to_be_bytes());
        sdt.push(0xFC);
        sdt.push(0x80 | ((desc.len() >> 8) as u8));
        sdt.push(desc.len() as u8);
        sdt.extend(desc);
    }
    let sdt = section(0x42, 0x0640, &sdt);
    ts.extend(packets(0x11, &sdt, &mut cc));
    // NIT: network name "Argentina", TS loop: ts 0x0640 with TS info (remote key 2) + partial reception 0xE778
    let mut nit = vec![];
    let nname = b"Argentina";
    let nd: Vec<u8> = [&[0x40, nname.len() as u8][..], nname].concat();
    nit.extend_from_slice(&[0xF0 | (nd.len() >> 8) as u8, nd.len() as u8]);
    nit.extend(&nd);
    let td = vec![0xCD, 2, 2, 0x00, 0xFB, 2, 0xE7, 0x78];
    let mut tsl = vec![0x06, 0x40, 0x06, 0x40, 0xF0, td.len() as u8];
    tsl.extend(td);
    nit.extend_from_slice(&[0xF0, tsl.len() as u8]);
    nit.extend(tsl);
    ts.extend(packets(0x10, &section(0x40, 0x0640, &nit), &mut cc));
    for _ in 0..5 {
        for p in [0x111, 0x112, 0x113, 0x811, 0x812] {
            ts.extend(es_packet(p));
        }
    }
    ts
}

#[test]
fn demux_learns_services() {
    let ts = mux();
    let mut d = Demux::new();
    let mut pk = Packetizer::default();
    for chunk in ts.chunks(1000) {
        pk.feed(chunk, |p| d.packet(p));
    }
    assert!(d.is_complete());
    assert_eq!(d.network_name, "Argentina");
    assert_eq!(d.ts_id, Some(0x0640));
    let s = d.services();
    assert_eq!(s.len(), 2);
    assert_eq!(s[0].name, "América HD");
    assert!(!s[0].one_seg);
    assert_eq!(s[0].virtual_number, "2.1");
    assert_eq!(s[0].streams.len(), 3);
    assert_eq!(s[1].name, "América 1seg");
    assert!(s[1].one_seg);
    assert_eq!(s[1].virtual_number, "2.2");
}

#[test]
fn filter_keeps_one_service() {
    let ts = mux();
    let mut d = Demux::new();
    let mut f = ServiceFilter::new(0xE760);
    let mut out = vec![];
    let mut pk = Packetizer::default();
    pk.feed(&ts, |p| {
        d.packet(p);
        f.update(&d);
        f.packet(p, &mut out);
    });
    assert!(f.is_ready());
    let pids: Vec<u16> = out.chunks(PKT).map(pid).collect();
    assert_eq!(pids[0], 0, "a PAT comes first");
    assert!(pids.contains(&0x111) && pids.contains(&0x112));
    assert!(!pids.contains(&0x113), "data stream dropped");
    assert!(
        !pids.contains(&0x811) && !pids.contains(&0x1FC8),
        "other service dropped"
    );
    // the injected PAT has a single program pointing at PMT 0x1F0
    let mut d2 = Demux::new();
    for p in out.chunks(PKT) {
        d2.packet(p);
    }
    let s = d2.services();
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].service_id, 0xE760);
    assert_eq!(s[0].pmt_pid, Some(0x1F0));
}
