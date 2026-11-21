//! Shopping Sessions ETL. See docs/DESIGN.md for the relational design and
//! docs/rules.md for the canonical semantics. Logging goes to stderr only.
//!
//! Plan (every step is a hash-partitioned, embarrassingly parallel operator):
//!   1. reference tables  products(product_id PK -> category), campaigns(campaign_id PK -> channel)
//!   2. identity          links -> union-find -> person name = min(member IDs)
//!   3. clicks            scan -> partition(click_id) -> dedup -> select -> partition(person)
//!   4. events            scan -> partition(event_id) -> dedup -> select -> partition(person)
//!   5. per person part   sort(person, ts) -> sessionize -> attribute -> aggregate
//!   6. output            user_daily(user_id, day) PK, campaign_daily(campaign_id, day) PK

mod dialect;
mod hashing;
mod output;
mod timefmt;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

use std::hash::BuildHasher;

use hashing::Seeded as RandomState;
use rayon::prelude::*;

use dialect::{Source, chunks, csv_files, data_region, for_each_line, header_order, split};
use timefmt::{BAD, DAY};

pub type Map<K, V> = std::collections::HashMap<K, V, RandomState>;

/// Partitioning hash (bucket choice) and table hash (inside a bucket) use different
/// seeds: keys of one bucket share their partition bits, and reusing them for the
/// table would cluster hashbrown's control bytes.
const PART_SEED: u64 = 0x243F_6A88_85A3_08D3;
const TABLE_SEED: u64 = 0x4528_21E6_38D0_1377;
pub fn table_state() -> RandomState {
    RandomState(TABLE_SEED)
}
pub fn new_map<K, V>(cap: usize) -> Map<K, V> {
    Map::with_capacity_and_hasher(cap, table_state())
}

const NB: usize = 512; // number of hash partitions (power of two)
const CHUNK: usize = 4 << 20; // scan granule, bytes
const WEEK: i64 = 7 * DAY;
const GAP: i64 = 30 * 60 * 1000;
pub const ORGANIC: u32 = u32::MAX;
pub const NO_CAT: u32 = u32::MAX;

#[inline]
fn part_of(state: &RandomState, key: &[u8]) -> usize {
    (state.hash_one(key) >> 20) as usize & (NB - 1)
}

type S = &'static [u8];

/// Counters: the data profile of a run (written to stderr, and to $STATS_FILE if set).
#[derive(Default)]
pub struct Stats {
    pub c: [AtomicU64; 32],
}
#[derive(Clone, Copy)]
#[repr(usize)]
pub enum K {
    EvLines, EvMalformed, EvNoId, EvDupCopies, EvKeptBad, EvBadUser, EvBadTs, EvBadType, EvBadProduct,
    EvBadPrice, EvLate, EvUnknownProduct, EvSurvived,
    ClLines, ClMalformed, ClNoId, ClDupCopies, ClBadUser, ClBadTs, ClBadCampaign, ClSurvived,
    LkLines, LkMalformed, LkEmptySide, LkKept, LkIds, LkPeopleMultiId, LkLargestPerson,
    Sessions, CreditedSessions, AssistPairs, Persons,
}
const KNAMES: [&str; 32] = [
    "events.lines", "events.malformed", "events.empty_event_id", "events.duplicate_copies_removed",
    "events.kept_copy_invalid", "events.bad_user", "events.bad_ts", "events.bad_type", "events.bad_product",
    "events.bad_price", "events.late", "events.unknown_product_kept", "events.survived",
    "clicks.lines", "clicks.malformed", "clicks.empty_click_id", "clicks.duplicate_copies_removed",
    "clicks.bad_user", "clicks.bad_ts", "clicks.bad_campaign", "clicks.survived",
    "links.lines", "links.malformed", "links.empty_side", "links.kept", "links.distinct_ids",
    "links.people_with_multiple_ids", "links.largest_person_ids",
    "sessions.total", "sessions.credited_non_organic", "sessions.assist_pairs", "persons.with_sessions",
];
impl Stats {
    #[inline]
    pub fn add(&self, k: K, v: u64) {
        if v != 0 {
            self.c[k as usize].fetch_add(v, Relaxed);
        }
    }
    fn get(&self, k: K) -> u64 {
        self.c[k as usize].load(Relaxed)
    }
    fn json(&self) -> String {
        let parts: Vec<String> =
            KNAMES.iter().enumerate().map(|(i, n)| format!("  \"{n}\": {}", self.c[i].load(Relaxed))).collect();
        format!("{{\n{}\n}}\n", parts.join(",\n"))
    }
}

fn env_or(k: &str, d: &str) -> String {
    std::env::var(k).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| d.to_string())
}

fn open_all(paths: &[PathBuf]) -> Vec<&'static Source> {
    paths
        .par_iter()
        .map(|p| {
            let s = Source::open(p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()));
            &*Box::leak(Box::new(s))
        })
        .collect()
}

/// (file header order, chunk) work items for a folder of part files.
fn work_items<const N: usize>(sources: &[&'static Source], names: [&str; N]) -> Vec<([usize; N], S)> {
    let mut out = Vec::new();
    for s in sources {
        let bytes: S = s.bytes();
        let order = header_order::<N>(bytes, names);
        for c in chunks(data_region(bytes), CHUNK) {
            out.push((order, c));
        }
    }
    out
}

#[inline]
fn reorder<const N: usize>(f: [S; N], order: &[usize; N]) -> [S; N] {
    let mut out = f;
    for i in 0..N {
        out[i] = f[order[i]];
    }
    out
}

fn main() {
    let t0 = Instant::now();
    let data = PathBuf::from(env_or("DATA_DIR", "/data"));
    let out = PathBuf::from(env_or("OUTPUT_DIR", "/output"));
    let ncpu: usize = env_or("NCPU", "0").parse().unwrap_or(0);
    let ncpu = if ncpu == 0 { std::thread::available_parallelism().map_or(4, |n| n.get()) } else { ncpu };
    rayon::ThreadPoolBuilder::new().num_threads(ncpu).build_global().expect("thread pool");
    let stats = Stats::default();
    let lap = |what: &str| eprintln!("[{:>7.3}s] {what}", t0.elapsed().as_secs_f64());

    // ---------------------------------------------------------------- manifest
    let manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(data.join("manifest.json")).expect("manifest.json"),
    )
    .expect("manifest.json is not JSON");
    let wget = |k: &str| -> i64 {
        let s = manifest[k].as_str().unwrap_or_else(|| panic!("manifest: {k} missing"));
        timefmt::parse(s.as_bytes()).unwrap_or_else(|| panic!("manifest: {k}={s:?} is not a valid timestamp"))
    };
    let (ws, we) = (wget("window_start"), wget("window_end"));
    eprintln!("window [{ws}, {we}) ms, threads {ncpu}");

    // ---------------------------------------------------------------- reference tables
    let mut files: Vec<PathBuf> = vec![data.join("products.csv"), data.join("campaigns.csv")];
    let ev_paths = csv_files(&data.join("events"));
    let cl_paths = csv_files(&data.join("clicks"));
    let lk_paths = csv_files(&data.join("links"));
    files.extend(ev_paths.iter().cloned());
    files.extend(cl_paths.iter().cloned());
    files.extend(lk_paths.iter().cloned());
    let all = open_all(&files);
    let (prod_src, camp_src) = (all[0], all[1]);
    let ev_src = &all[2..2 + ev_paths.len()];
    let cl_src = &all[2 + ev_paths.len()..2 + ev_paths.len() + cl_paths.len()];
    let lk_src = &all[2 + ev_paths.len() + cl_paths.len()..];
    eprintln!("files: events {} clicks {} links {}", ev_src.len(), cl_src.len(), lk_src.len());

    let (products, cat_names) = load_products(prod_src);
    let (campaigns, channels, camp_ids) = load_campaigns(camp_src);
    let unknown_cat = cat_names.iter().position(|c| *c == b"unknown").expect("unknown category") as u32;
    lap("reference tables");

    // ---------------------------------------------------------------- identity
    let names = resolve_links(lk_src, &stats);
    lap("links resolved");
    let person_of = |user: S| -> S { names.get(user).copied().unwrap_or(user) };
    let part_state = RandomState(PART_SEED);

    // ---------------------------------------------------------------- clicks
    let clicks_by_person = clicks(cl_src, &campaigns, ws - WEEK, we, &person_of, &part_state, &stats);
    lap("clicks");

    // ---------------------------------------------------------------- events
    let events_by_person = events(ev_src, &products, unknown_cat, ws, we, &person_of, &part_state, &stats);
    lap("events");

    // ---------------------------------------------------------------- sessions
    let ncamp = camp_ids.len();
    let results: Vec<(Vec<output::UserRow>, Map<(u32, i32), [u64; 4]>)> = (0..NB)
        .into_par_iter()
        .map(|b| sessionize(b, &events_by_person, &clicks_by_person, ncamp, &stats))
        .collect();
    lap("sessions");

    let mut camp_agg: Map<(u32, i32), [u64; 4]> = new_map(1 << 14);
    let mut user_parts = Vec::with_capacity(NB);
    for (u, c) in results {
        user_parts.push(u);
        for (k, v) in c {
            let e = camp_agg.entry(k).or_insert([0; 4]);
            for i in 0..4 {
                e[i] += v[i];
            }
        }
    }
    drop(events_by_person);
    drop(clicks_by_person);

    // ---------------------------------------------------------------- output
    output::write_all(&out, user_parts, camp_agg, &cat_names, &camp_ids, &channels, ncpu);
    lap("outputs written");

    let json = stats.json();
    eprint!("stats {json}");
    if let Ok(p) = std::env::var("STATS_FILE") {
        let _ = std::fs::write(p, &json);
    }
    // cheap self-check from rules.md: assists >= credited non-organic sessions
    assert!(stats.get(K::AssistPairs) >= stats.get(K::CreditedSessions), "assist invariant violated");
    lap("done");
}

// ====================================================================== reference tables

/// products(product_id PK, category). First occurrence wins if a key repeats.
/// Categories are numbered in byte order, so "smallest index" == "byte-wise smallest name".
fn load_products(src: &'static Source) -> (Map<S, u32>, Vec<S>) {
    let bytes: S = src.bytes();
    let order = header_order::<2>(bytes, ["product_id", "category"]);
    let mut rows: Vec<(S, S)> = Vec::new();
    for_each_line(data_region(bytes), |line| {
        if let Some(f) = split::<2>(line) {
            let f = reorder(f, &order);
            if !f[0].is_empty() {
                rows.push((f[0], f[1]));
            }
        }
    });
    let mut cats: Vec<S> = rows.iter().map(|r| r.1).collect();
    cats.push(b"unknown");
    cats.sort_unstable();
    cats.dedup();
    let mut idx: Map<S, u32> = new_map(cats.len());
    for (i, c) in cats.iter().enumerate() {
        idx.insert(*c, i as u32);
    }
    let mut products: Map<S, u32> = new_map(rows.len());
    for (p, c) in rows {
        products.entry(p).or_insert(idx[c]);
    }
    (products, cats)
}

/// campaigns(campaign_id PK, channel). Returns id -> dense index, channel per index, id per index.
fn load_campaigns(src: &'static Source) -> (Map<S, u32>, Vec<S>, Vec<S>) {
    let bytes: S = src.bytes();
    let order = header_order::<2>(bytes, ["campaign_id", "channel"]);
    let mut m: Map<S, u32> = new_map(4096);
    let (mut ch, mut ids) = (Vec::new(), Vec::new());
    for_each_line(data_region(bytes), |line| {
        if let Some(f) = split::<2>(line) {
            let f = reorder(f, &order);
            if !f[0].is_empty() && !m.contains_key(f[0]) {
                m.insert(f[0], ids.len() as u32);
                ids.push(f[0]);
                ch.push(f[1]);
            }
        }
    });
    (m, ch, ids)
}

// ====================================================================== identity

/// Connected components over link edges (union-find with path halving + union by size).
/// Returns only the IDs whose person name differs from the ID itself.
fn resolve_links(src: &[&'static Source], stats: &Stats) -> Map<S, S> {
    let items = work_items::<2>(src, ["user_id_a", "user_id_b"]);
    let edges: Vec<Vec<(S, S)>> = items
        .par_iter()
        .map(|(order, chunk)| {
            let mut v = Vec::new();
            let (mut lines, mut bad, mut empty) = (0u64, 0u64, 0u64);
            for_each_line(chunk, |line| {
                lines += 1;
                match split::<2>(line) {
                    None => bad += 1,
                    Some(f) => {
                        let f = reorder(f, order);
                        if f[0].is_empty() || f[1].is_empty() {
                            empty += 1;
                        } else {
                            v.push((f[0], f[1]));
                        }
                    }
                }
            });
            stats.add(K::LkLines, lines);
            stats.add(K::LkMalformed, bad);
            stats.add(K::LkEmptySide, empty);
            stats.add(K::LkKept, v.len() as u64);
            v
        })
        .collect();
    let n_edges: usize = edges.iter().map(Vec::len).sum();
    let mut id: Map<S, u32> = new_map(n_edges * 2);
    let mut ids: Vec<S> = Vec::with_capacity(n_edges * 2);
    let mut parent: Vec<u32> = Vec::with_capacity(n_edges * 2);
    let mut size: Vec<u32> = Vec::with_capacity(n_edges * 2);
    let mut intern = |s: S, ids: &mut Vec<S>, parent: &mut Vec<u32>, size: &mut Vec<u32>| -> u32 {
        *id.entry(s).or_insert_with(|| {
            let i = ids.len() as u32;
            ids.push(s);
            parent.push(i);
            size.push(1);
            i
        })
    };
    fn find(parent: &mut [u32], mut x: u32) -> u32 {
        while parent[x as usize] != x {
            let g = parent[parent[x as usize] as usize];
            parent[x as usize] = g;
            x = g;
        }
        x
    }
    for chunk in &edges {
        for &(a, b) in chunk {
            let ia = intern(a, &mut ids, &mut parent, &mut size);
            let ib = intern(b, &mut ids, &mut parent, &mut size);
            let (ra, rb) = (find(&mut parent, ia), find(&mut parent, ib));
            if ra != rb {
                let (big, small) = if size[ra as usize] >= size[rb as usize] { (ra, rb) } else { (rb, ra) };
                parent[small as usize] = big;
                size[big as usize] += size[small as usize];
            }
        }
    }
    let n = ids.len();
    let mut name: Vec<u32> = (0..n as u32).collect(); // per root: index of the smallest member
    let mut root = vec![0u32; n];
    for i in 0..n {
        let r = find(&mut parent, i as u32);
        root[i] = r;
        if ids[i] < ids[name[r as usize] as usize] {
            name[r as usize] = i as u32;
        }
    }
    let mut out: Map<S, S> = new_map(n);
    let mut multi = 0u64;
    let mut largest = 0u64;
    for i in 0..n {
        let r = root[i] as usize;
        if r == i {
            if size[r] > 1 {
                multi += 1;
            }
            largest = largest.max(size[r] as u64);
        }
        let nm = ids[name[r] as usize];
        if nm != ids[i] {
            out.insert(ids[i], nm);
        }
    }
    stats.add(K::LkIds, n as u64);
    stats.add(K::LkPeopleMultiId, multi);
    stats.add(K::LkLargestPerson, largest);
    out
}

// ====================================================================== clicks

#[derive(Clone, Copy)]
struct ClickRaw {
    cid: S,
    user: S,
    camp: S,
    ts: S,
}

#[derive(Clone, Copy)]
pub struct ClickV {
    pub person: S,
    pub ts: i64,
    pub cid: S,
    pub camp: u32,
}

/// Partitioned by person, as [person partition][source partition] -> rows.
type ByPerson<T> = Vec<Vec<Vec<T>>>;

fn transpose<T>(v: Vec<Vec<Vec<T>>>) -> ByPerson<T> {
    // v[src][dst] -> out[dst][src]
    let mut out: ByPerson<T> = (0..NB).map(|_| Vec::with_capacity(v.len())).collect();
    for row in v {
        for (dst, part) in row.into_iter().enumerate() {
            out[dst].push(part);
        }
    }
    out
}

fn clicks(
    src: &[&'static Source],
    campaigns: &Map<S, u32>,
    lo: i64,
    hi: i64,
    person_of: &(dyn Fn(S) -> S + Sync),
    ps: &RandomState,
    stats: &Stats,
) -> ByPerson<ClickV> {
    let items = work_items::<4>(src, ["click_id", "user_id", "campaign_id", "ts"]);
    // scan + partition on click_id
    let scattered: Vec<Vec<Vec<ClickRaw>>> = items
        .par_iter()
        .map(|(order, chunk)| {
            let mut parts: Vec<Vec<ClickRaw>> = (0..NB).map(|_| Vec::new()).collect();
            let (mut lines, mut bad, mut noid) = (0u64, 0u64, 0u64);
            for_each_line(chunk, |line| {
                lines += 1;
                let Some(f) = split::<4>(line) else {
                    bad += 1;
                    return;
                };
                let f = reorder(f, order);
                if f[0].is_empty() {
                    noid += 1;
                    return;
                }
                parts[part_of(ps, f[0])].push(ClickRaw { cid: f[0], user: f[1], camp: f[2], ts: f[3] });
            });
            stats.add(K::ClLines, lines);
            stats.add(K::ClMalformed, bad);
            stats.add(K::ClNoId, noid);
            parts
        })
        .collect();
    let by_cid = transpose(scattered);
    // dedup on click_id (copies are identical; keep the smallest tuple for determinism),
    // then validate, then re-partition on person
    let out: Vec<Vec<Vec<ClickV>>> = by_cid
        .into_par_iter()
        .map(|parts| {
            let n: usize = parts.iter().map(Vec::len).sum();
            let mut best: Map<S, ClickRaw> = new_map(n);
            for part in &parts {
                for r in part {
                    best.entry(r.cid)
                        .and_modify(|cur| {
                            if (r.user, r.camp, r.ts) < (cur.user, cur.camp, cur.ts) {
                                *cur = *r;
                            }
                        })
                        .or_insert(*r);
                }
            }
            stats.add(K::ClDupCopies, (n - best.len()) as u64);
            let mut dst: Vec<Vec<ClickV>> = (0..NB).map(|_| Vec::new()).collect();
            let (mut bu, mut bt, mut bc) = (0u64, 0u64, 0u64);
            for r in best.values() {
                if r.user.is_empty() {
                    bu += 1;
                    continue;
                }
                let ts = timefmt::parse_in(r.ts, lo, hi);
                if ts == BAD {
                    bt += 1;
                    continue;
                }
                let Some(&camp) = campaigns.get(r.camp) else {
                    bc += 1;
                    continue;
                };
                let person = person_of(r.user);
                dst[part_of(ps, person)].push(ClickV { person, ts, cid: r.cid, camp });
            }
            stats.add(K::ClBadUser, bu);
            stats.add(K::ClBadTs, bt);
            stats.add(K::ClBadCampaign, bc);
            stats.add(K::ClSurvived, dst.iter().map(|v| v.len() as u64).sum());
            dst
        })
        .collect();
    transpose(out)
}

// ====================================================================== events

const T_VIEW: u8 = 0;
const T_CART: u8 = 1;
const T_PURCHASE: u8 = 2;
const T_BAD: u8 = 3;

#[derive(Clone, Copy)]
struct EvRaw {
    eid: S,
    line_fields: S, // the whole line, re-split only to break an ingest_ts tie
    person: S,      // empty if user_id was empty
    ts: i64,        // BAD if invalid or outside the event window
    ingest: i64,
    cat: u32,   // NO_CAT if product_id empty
    price: u32, // 0 if not a valid purchase price (or not a purchase)
    typ: u8,
    ingest_bad: bool,
    unknown_product: bool,
}

#[derive(Clone, Copy)]
pub struct EvV {
    pub person: S,
    pub ts: i64,
    pub cat: u32,
    pub price: u32, // > 0 iff purchase
}

#[inline]
fn parse_price(b: &[u8]) -> u32 {
    if b.is_empty() || !b.iter().all(u8::is_ascii_digit) {
        return 0;
    }
    let first = b.iter().position(|&c| c != b'0').unwrap_or(b.len());
    let d = &b[first..];
    if d.len() > 8 {
        return 0;
    }
    let mut v: u32 = 0;
    for &c in d {
        v = v * 10 + (c - b'0') as u32;
    }
    if (1..=10_000_000).contains(&v) { v } else { 0 }
}

/// ingest_ts is promised valid F1; parse every form anyway. An F3 too large for i64
/// saturates (it is still "a number", just enormous).
#[inline]
fn parse_ingest(b: &[u8]) -> Option<i64> {
    timefmt::parse(b).or_else(|| (!b.is_empty() && b.iter().all(u8::is_ascii_digit)).then_some(i64::MAX))
}

#[allow(clippy::too_many_arguments)]
fn events(
    src: &[&'static Source],
    products: &Map<S, u32>,
    unknown_cat: u32,
    lo: i64,
    hi: i64,
    person_of: &(dyn Fn(S) -> S + Sync),
    ps: &RandomState,
    stats: &Stats,
) -> ByPerson<EvV> {
    const NAMES: [&str; 7] = ["event_id", "user_id", "ts", "ingest_ts", "event_type", "product_id", "price_cents"];
    let items = work_items::<7>(src, NAMES);
    let scattered: Vec<Vec<Vec<EvRaw>>> = items
        .par_iter()
        .map(|(order, chunk)| {
            let mut parts: Vec<Vec<EvRaw>> = (0..NB).map(|_| Vec::with_capacity(chunk.len() / 80 / NB + 8)).collect();
            let (mut lines, mut bad, mut noid) = (0u64, 0u64, 0u64);
            for_each_line(chunk, |line| {
                lines += 1;
                let Some(f) = split::<7>(line) else {
                    bad += 1;
                    return;
                };
                let f = reorder(f, order);
                if f[0].is_empty() {
                    noid += 1;
                    return;
                }
                let typ = match f[4] {
                    b"view" => T_VIEW,
                    b"cart" => T_CART,
                    b"purchase" => T_PURCHASE,
                    _ => T_BAD,
                };
                let (cat, unknown_product) = if f[5].is_empty() {
                    (NO_CAT, false)
                } else {
                    match products.get(f[5]) {
                        Some(&c) => (c, false),
                        None => (unknown_cat, true),
                    }
                };
                let ingest = parse_ingest(f[3]);
                parts[part_of(ps, f[0])].push(EvRaw {
                    eid: f[0],
                    line_fields: line,
                    person: if f[1].is_empty() { b"" } else { person_of(f[1]) },
                    ts: timefmt::parse_in(f[2], lo, hi),
                    ingest: ingest.unwrap_or(i64::MAX),
                    cat,
                    price: if typ == T_PURCHASE { parse_price(f[6]) } else { 0 },
                    typ,
                    ingest_bad: ingest.is_none(),
                    unknown_product,
                });
            });
            stats.add(K::EvLines, lines);
            stats.add(K::EvMalformed, bad);
            stats.add(K::EvNoId, noid);
            parts
        })
        .collect();
    let by_eid = transpose(scattered);
    let out: Vec<Vec<Vec<EvV>>> = by_eid
        .into_par_iter()
        .map(|parts| {
            let n: usize = parts.iter().map(Vec::len).sum();
            let mut best: Map<S, EvRaw> = new_map(n);
            for part in &parts {
                for r in part {
                    match best.get_mut(r.eid) {
                        None => {
                            best.insert(r.eid, *r);
                        }
                        Some(cur) => {
                            if earlier(r, cur) {
                                *cur = *r;
                            }
                        }
                    }
                }
            }
            stats.add(K::EvDupCopies, (n - best.len()) as u64);
            let mut dst: Vec<Vec<EvV>> = (0..NB).map(|_| Vec::with_capacity(n / NB + 8)).collect();
            let mut c = [0u64; 8];
            for r in best.values() {
                // validation of the kept copy, in the order of rules.md
                let reason = if r.person.is_empty() {
                    0
                } else if r.ts == BAD {
                    1
                } else if r.typ == T_BAD {
                    2
                } else if r.cat == NO_CAT {
                    3
                } else if r.typ == T_PURCHASE && r.price == 0 {
                    4
                } else if r.ingest_bad || r.ingest.saturating_sub(r.ts) > DAY {
                    5
                } else {
                    if r.unknown_product {
                        c[6] += 1;
                    }
                    dst[part_of(ps, r.person)].push(EvV { person: r.person, ts: r.ts, cat: r.cat, price: r.price });
                    continue;
                };
                c[reason] += 1;
            }
            for (k, i) in [(K::EvBadUser, 0), (K::EvBadTs, 1), (K::EvBadType, 2), (K::EvBadProduct, 3), (K::EvBadPrice, 4), (K::EvLate, 5), (K::EvUnknownProduct, 6)] {
                stats.add(k, c[i]);
            }
            stats.add(K::EvKeptBad, c[..5].iter().sum());
            stats.add(K::EvSurvived, dst.iter().map(|v| v.len() as u64).sum());
            dst
        })
        .collect();
    transpose(out)
}

/// Is copy `a` preferred over `b` in dedup? Smallest parseable ingest_ts first
/// (unparseable ranks last); equal ingest_ts copies are promised identical, so the
/// tie is broken by the raw field tuple purely for determinism.
#[inline]
fn earlier(a: &EvRaw, b: &EvRaw) -> bool {
    let ka = (a.ingest_bad, a.ingest);
    let kb = (b.ingest_bad, b.ingest);
    if ka != kb {
        return ka < kb;
    }
    let fa = split::<7>(a.line_fields).unwrap();
    let fb = split::<7>(b.line_fields).unwrap();
    fa < fb
}

// ====================================================================== sessions

fn sessionize(
    b: usize,
    evs: &ByPerson<EvV>,
    cls: &ByPerson<ClickV>,
    ncamp: usize,
    stats: &Stats,
) -> (Vec<output::UserRow>, Map<(u32, i32), [u64; 4]>) {
    let n: usize = evs[b].iter().map(Vec::len).sum();
    let mut pid: Map<S, u32> = new_map(n / 8 + 16);
    let mut pname: Vec<S> = Vec::new();
    // (person, ts, cat, price)
    let mut ev: Vec<(u32, i64, u32, u32)> = Vec::with_capacity(n);
    for part in &evs[b] {
        for e in part {
            let p = *pid.entry(e.person).or_insert_with(|| {
                pname.push(e.person);
                (pname.len() - 1) as u32
            });
            ev.push((p, e.ts, e.cat, e.price));
        }
    }
    ev.sort_unstable_by_key(|e| (e.0, e.1));
    // clicks of persons that have events; sorted by (person, ts asc, click_id desc) so the
    // last click of a window is the latest one with the smallest click_id
    let mut cl: Vec<(u32, i64, S, u32)> = Vec::new();
    for part in &cls[b] {
        for c in part {
            if let Some(&p) = pid.get(c.person) {
                cl.push((p, c.ts, c.cid, c.camp));
            }
        }
    }
    cl.sort_unstable_by(|x, y| (x.0, x.1).cmp(&(y.0, y.1)).then_with(|| y.2.cmp(x.2)));

    let mut rows: Vec<output::UserRow> = Vec::with_capacity(pname.len() * 2);
    let mut camp: Map<(u32, i32), [u64; 4]> = new_map(1 << 12);
    // sliding-window distinct-campaign set: count per campaign + list of active ones
    let mut cnt = vec![0u32; ncamp];
    let mut pos = vec![u32::MAX; ncamp];
    let mut active: Vec<u32> = Vec::new();
    let (mut sessions_total, mut credited, mut assists) = (0u64, 0u64, 0u64);

    let mut i = 0usize; // event cursor
    let mut j = 0usize; // click cursor (first click of current person)
    let mut cat_rev: Vec<(u32, u64)> = Vec::new();
    while i < ev.len() {
        let p = ev[i].0;
        let pe = i + ev[i..].iter().position(|e| e.0 != p).unwrap_or(ev.len() - i);
        while j < cl.len() && cl[j].0 < p {
            j += 1;
        }
        let pc = j + cl[j..].iter().position(|c| c.0 != p).unwrap_or(cl.len() - j);
        let (mut lo, mut hi) = (j, j); // window [lo, hi) of this person's clicks

        // current (person, day) accumulation
        let mut day_cur: i32 = i32::MIN;
        let (mut d_sessions, mut d_events, mut d_rev) = (0u64, 0u64, 0u64);
        cat_rev.clear();

        let mut k = i;
        while k < pe {
            // one session: [k, e)
            let start = ev[k].1;
            let mut e = k + 1;
            while e < pe && ev[e].1 - ev[e - 1].1 <= GAP {
                e += 1;
            }
            let day = start.div_euclid(DAY) as i32;
            let (mut rev, mut has_p) = (0u64, false);
            for x in &ev[k..e] {
                if x.3 > 0 {
                    has_p = true;
                    rev += x.3 as u64;
                }
            }
            // attribution window: clicks with start - 7d <= ts <= start
            while hi < pc && cl[hi].1 <= start {
                let c = cl[hi].3 as usize;
                if cnt[c] == 0 {
                    pos[c] = active.len() as u32;
                    active.push(c as u32);
                }
                cnt[c] += 1;
                hi += 1;
            }
            while lo < hi && cl[lo].1 < start - WEEK {
                let c = cl[lo].3 as usize;
                cnt[c] -= 1;
                if cnt[c] == 0 {
                    let at = pos[c] as usize;
                    let last = *active.last().unwrap();
                    active[at] = last;
                    pos[last as usize] = at as u32;
                    active.pop();
                    pos[c] = u32::MAX;
                }
                lo += 1;
            }
            let credit = if hi > lo { cl[hi - 1].3 } else { ORGANIC };
            let a = camp.entry((credit, day)).or_insert([0; 4]);
            a[0] += 1;
            a[1] += has_p as u64;
            a[2] += rev;
            for &c in &active {
                camp.entry((c, day)).or_insert([0; 4])[3] += 1;
            }
            sessions_total += 1;
            credited += (credit != ORGANIC) as u64;
            assists += active.len() as u64;

            // user_daily accumulation
            if day != day_cur {
                if day_cur != i32::MIN {
                    rows.push(output::UserRow::new(pname[p as usize], day_cur, d_sessions, d_events, d_rev, &cat_rev));
                }
                day_cur = day;
                d_sessions = 0;
                d_events = 0;
                d_rev = 0;
                cat_rev.clear();
            }
            d_sessions += 1;
            d_events += (e - k) as u64;
            d_rev += rev;
            if has_p {
                for x in &ev[k..e] {
                    if x.3 > 0 {
                        match cat_rev.iter_mut().find(|c| c.0 == x.2) {
                            Some(c) => c.1 += x.3 as u64,
                            None => cat_rev.push((x.2, x.3 as u64)),
                        }
                    }
                }
            }
            k = e;
        }
        rows.push(output::UserRow::new(pname[p as usize], day_cur, d_sessions, d_events, d_rev, &cat_rev));
        // reset the window set for the next person
        for &c in &active {
            cnt[c as usize] = 0;
            pos[c as usize] = u32::MAX;
        }
        active.clear();
        i = pe;
        j = pc;
    }
    stats.add(K::Sessions, sessions_total);
    stats.add(K::CreditedSessions, credited);
    stats.add(K::AssistPairs, assists);
    stats.add(K::Persons, pname.len() as u64);
    (rows, camp)
}

