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
mod keys;
mod output;
mod packed;
mod timefmt;

use std::collections::hash_map::Entry;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

use std::hash::BuildHasher;

use hashing::Seeded as RandomState;
use rayon::prelude::*;

use dialect::{Source, chunks, csv_files, data_region, for_each_line, for_each_record, header_order, split};
use keys::{KeyMap, key_of};
use packed::{PMap, pack9};
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

const LOG_NB: u32 = 8;
const NB: usize = 1 << LOG_NB;
// NB: number of hash partitions; night-11 found 256 best (RW1 keeps it). Partitions take
// the top LOG_NB bits of fmix(key) (keys::part), disjoint from table-slot bits.
const CHUNK: usize = 4 << 20; // scan granule, bytes
const WEEK: i64 = 7 * DAY;
const GAP: i64 = 30 * 60 * 1000;
pub const ORGANIC: u32 = u32::MAX;
pub const NO_CAT: u32 = u32::MAX;

#[inline]
fn part_of(state: &RandomState, key: &[u8]) -> usize {
    (state.hash_one(key) >> 20) as usize & (NB - 1)
}

#[inline]
fn part_of_u64(state: &RandomState, key: u64) -> usize {
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

/// Sub-stage timer for profiling (stderr only; never changes output).
struct Stage(Instant);
impl Stage {
    fn start() -> Stage {
        Stage(Instant::now())
    }
    fn mark(&mut self, what: &str) {
        eprintln!("    {what:<34} {:>7.3}s", self.0.elapsed().as_secs_f64());
        self.0 = Instant::now();
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
    if std::env::var("SS_EXP").is_ok() {
        experiment(ev_src, &products, ws, we, &names);
        return;
    }

    // ---------------------------------------------------------------- clicks
    let clicks_by_person = clicks(cl_src, &campaigns, ws - WEEK, we, &names, &stats);
    lap("clicks");

    // ---------------------------------------------------------------- events
    let events_by_person = events(ev_src, &products, unknown_cat, ws, we, &names, &stats);
    lap("events");

    // ---------------------------------------------------------------- sessions
    // Night-01: campaign/day accumulators are a dense array per rayon fold (campaign
    // index, ORGANIC last; day offset from the first window day) instead of a hash map.
    let ncamp = camp_ids.len();
    let day0 = ws.div_euclid(DAY) as i32;
    let ndays = ((we - 1).div_euclid(DAY) as i32 - day0 + 1).max(1) as usize;
    let dense_len = (ncamp + 1) * ndays;
    let folded: Vec<(Vec<Vec<output::UserRow>>, Vec<[u64; 4]>)> = (0..NB)
        .into_par_iter()
        .fold(
            || (Vec::new(), Vec::new()),
            |(mut rows, mut acc): (Vec<Vec<output::UserRow>>, Vec<[u64; 4]>), b| {
                if acc.is_empty() {
                    acc = vec![[0u64; 4]; dense_len];
                }
                rows.push(sessionize(b, &events_by_person, &clicks_by_person, ncamp, day0, ndays, &mut acc, &stats));
                (rows, acc)
            },
        )
        .collect();
    let mut dense = vec![[0u64; 4]; dense_len];
    let mut user_parts = Vec::with_capacity(NB);
    for (rows, acc) in folded {
        user_parts.extend(rows);
        for (d, a) in dense.iter_mut().zip(acc.iter()) {
            for i in 0..4 {
                d[i] += a[i];
            }
        }
    }
    let mut camp_agg: Map<(u32, i32), [u64; 4]> = new_map(1 << 14);
    for (idx, v) in dense.iter().enumerate() {
        if v[0] != 0 || v[3] != 0 {
            let (c, d) = (idx / ndays, idx % ndays);
            let c = if c == ncamp { ORGANIC } else { c as u32 };
            camp_agg.insert((c, day0 + d as i32), *v);
        }
    }
    lap("sessions");

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

/// SS_EXP=1: single-thread cost of each scan layer over all event files (profiling only).
fn experiment(src: &[&'static Source], products: &PMap<u32>, lo: i64, hi: i64, names: &Names) {
    const NAMES: [&str; 7] = ["event_id", "user_id", "ts", "ingest_ts", "event_type", "product_id", "price_cents"];
    let items = work_items::<7>(src, NAMES);
    let bytes: usize = items.iter().map(|(_, c)| c.len()).sum();
    let mut lines = 0u64;
    for (_, c) in &items {
        for_each_line(c, |_| lines += 1);
    }
    let run = |name: &str, f: &dyn Fn(&[S; 7]) -> u64| {
        let t = Instant::now();
        let mut acc = 0u64;
        for (order, c) in &items {
            for_each_record::<7>(c, |rec| {
                if let Some(r) = rec {
                    acc = acc.wrapping_add(f(&reorder(r, order)));
                }
            });
        }
        let secs = t.elapsed().as_secs_f64();
        eprintln!("{name:<28} {secs:>6.3}s  {:>6.1} ns/line  {:>5.2} GB/s  (acc {acc})", secs * 1e9 / lines as f64, bytes as f64 / secs / 1e9);
    };
    let t = Instant::now();
    let mut n = 0u64;
    for (_, c) in &items {
        for_each_line(c, |l| n += l.len() as u64);
    }
    eprintln!("{:<28} {:>6.3}s  ({lines} lines, {:.2} GB)", "lines only (memchr)", t.elapsed().as_secs_f64(), bytes as f64 / 1e9);
    run("records (fused scanner)", &|f| f[0].len() as u64);
    run("+ key_of(eid)", &|f| key_of(f[0]));
    run("+ key_of(user)", &|f| key_of(f[0]) ^ key_of(f[1]));
    run("+ ts parse_in", &|f| key_of(f[0]) ^ key_of(f[1]) ^ timefmt::parse_in(f[2], lo, hi) as u64);
    run("+ ingest parse", &|f| key_of(f[0]) ^ key_of(f[1]) ^ timefmt::parse_in(f[2], lo, hi) as u64 ^ parse_ingest(f[3]).unwrap_or(0) as u64);
    run("+ type + price", &|f| {
        key_of(f[0]) ^ key_of(f[1]) ^ timefmt::parse_in(f[2], lo, hi) as u64 ^ parse_ingest(f[3]).unwrap_or(0) as u64
            ^ (f[4] == b"purchase") as u64 ^ parse_price(f[6]) as u64
    });
    run("+ person lookup", &|f| {
        key_of(f[0]) ^ names.person(key_of(f[1])) ^ timefmt::parse_in(f[2], lo, hi) as u64 ^ parse_ingest(f[3]).unwrap_or(0) as u64
            ^ (f[4] == b"purchase") as u64 ^ parse_price(f[6]) as u64 ^ products.get(f[5]).copied().unwrap_or(0) as u64
    });
    // + scatter: the same work, pushing a 40-byte record into one of NB partition vectors
    let t = Instant::now();
    let mut kept = 0usize;
    for (order, c) in &items {
        let mut parts: Vec<Vec<EvRec>> = (0..NB).map(|_| Vec::with_capacity(c.len() / 80 / NB + 8)).collect();
        for_each_record::<7>(c, |rec| {
            if let Some(r) = rec {
                let f = reorder(r, order);
                let eid = key_of(f[0]);
                let r = EvRec { eid, rank: rank_of(parse_ingest(f[3])), person: names.person(key_of(f[1])), ts: timefmt::parse_in(f[2], lo, hi),
                    price: parse_price(f[6]), cat: products.get(f[5]).copied().unwrap_or(0) };
                parts[keys::part(eid, LOG_NB)].push(r);
            }
        });
        kept += parts.iter().map(Vec::len).sum::<usize>();
    }
    let secs = t.elapsed().as_secs_f64();
    eprintln!("{:<28} {secs:>6.3}s  {:>6.1} ns/line  (kept {kept})", "+ person + scatter", secs * 1e9 / lines as f64);
    run("+ product lookup", &|f| {
        key_of(f[0]) ^ key_of(f[1]) ^ timefmt::parse_in(f[2], lo, hi) as u64 ^ parse_ingest(f[3]).unwrap_or(0) as u64
            ^ (f[4] == b"purchase") as u64 ^ parse_price(f[6]) as u64 ^ products.get(f[5]).copied().unwrap_or(0) as u64
    });
}

// ====================================================================== reference tables

/// products(product_id PK, category). First occurrence wins if a key repeats.
/// Categories are numbered in byte order, so "smallest index" == "byte-wise smallest name".
fn load_products(src: &'static Source) -> (PMap<u32>, Vec<S>) {
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
    let mut products: PMap<u32> = PMap::with_capacity(rows.len());
    for (p, c) in rows {
        products.insert_first(p, idx[c]);
    }
    (products, cats)
}

/// campaigns(campaign_id PK, channel). Returns id -> dense index, channel per index, id per index.
fn load_campaigns(src: &'static Source) -> (PMap<u32>, Vec<S>, Vec<S>) {
    let bytes: S = src.bytes();
    let order = header_order::<2>(bytes, ["campaign_id", "channel"]);
    let mut m: PMap<u32> = PMap::with_capacity(4096);
    let (mut ch, mut ids) = (Vec::new(), Vec::new());
    for_each_line(data_region(bytes), |line| {
        if let Some(f) = split::<2>(line) {
            let f = reorder(f, &order);
            if !f[0].is_empty() && !m.contains_key(f[0]) {
                m.insert_first(f[0], ids.len() as u32);
                ids.push(f[0]);
                ch.push(f[1]);
            }
        }
    });
    (m, ch, ids)
}

// ====================================================================== identity

/// Night-02: number of hash partitions for link IDs (interning and the name map).
const LP: usize = 64;
const LINK_SEED: u64 = 0x1319_8A2E_0370_7344;

/// Person keys of linked IDs, split into `LP` partitions so the tables build in parallel.
/// RW1: keyed by `keys::key_of(id)`, valued by the key of the person's name (the
/// byte-wise smallest member ID). Holds only IDs whose person differs from themselves.
pub struct Names {
    parts: Vec<KeyMap>,
}
/// RW2: rows waiting for their person lookup; the table slots of a whole batch are
/// prefetched before any lookup runs, so the cache misses overlap.
const PBATCH: usize = 32;
pub struct Pending<T> {
    rows: Vec<(T, u64, u64)>, // (row, user key, mix)
}
impl<T: Copy> Pending<T> {
    pub fn new() -> Self {
        Pending { rows: Vec::with_capacity(PBATCH) }
    }
    /// Queue a row; when the batch is full, resolve it: `done(row, person_key)`.
    #[inline]
    pub fn push(&mut self, names: &Names, row: T, user: u64, done: &mut impl FnMut(T, u64)) {
        let m = packed::fmix(user);
        names.prefetch(m);
        self.rows.push((row, user, m));
        if self.rows.len() == PBATCH {
            self.flush(names, done);
        }
    }
    pub fn flush(&mut self, names: &Names, done: &mut impl FnMut(T, u64)) {
        for &(row, user, m) in &self.rows {
            done(row, names.person_mixed(m, user));
        }
        self.rows.clear();
    }
}

impl Names {
    #[inline(always)]
    fn prefetch(&self, m: u64) {
        self.parts[(m >> 40) as usize & (LP - 1)].prefetch(m);
    }
    #[inline(always)]
    fn person_mixed(&self, m: u64, k: u64) -> u64 {
        self.parts[(m >> 40) as usize & (LP - 1)].get_mixed(m, k).unwrap_or(k)
    }
    /// The person key of a user key (EMPTY stays EMPTY).
    #[inline]
    pub fn person(&self, k: u64) -> u64 {
        let m = packed::fmix(k);
        match self.parts[(m >> 40) as usize & (LP - 1)].get_mixed(m, k) {
            Some(p) => p,
            None => k,
        }
    }
}

/// Connected components over link edges (union-find with path halving + union by size).
/// Night-02: IDs are interned in parallel, one table per hash partition; the union-find
/// itself then runs on dense integers. The name of a person is still the byte-wise
/// smallest member ID, so the result does not depend on partitioning or union order.
fn resolve_links(src: &[&'static Source], stats: &Stats) -> Names {
    let mut st = Stage::start();
    let lstate = RandomState(LINK_SEED);
    let part = |s: S| (packed::id_hash(&lstate, s) >> 40) as usize & (LP - 1);
    let items = work_items::<2>(src, ["user_id_a", "user_id_b"]);
    let edges: Vec<Vec<(S, S)>> = items
        .par_iter()
        .map(|(order, chunk)| {
            let mut v = Vec::new();
            let (mut lines, mut bad, mut empty) = (0u64, 0u64, 0u64);
            // night-04: fused one-pass line + field scanner (same dialect as split)
            for_each_record::<2>(chunk, |rec| {
                lines += 1;
                match rec {
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
    st.mark("links: parse");
    // global edge numbering: chunk offsets
    let mut eoff = Vec::with_capacity(edges.len() + 1);
    eoff.push(0usize);
    for c in &edges {
        eoff.push(eoff.last().unwrap() + c.len());
    }
    let n_edges = *eoff.last().unwrap();
    // scatter (id, slot) by partition, slot = 2 * edge + side
    let scattered: Vec<Vec<Vec<(S, u32)>>> = edges
        .par_iter()
        .enumerate()
        .map(|(ci, c)| {
            let mut out: Vec<Vec<(S, u32)>> = (0..LP).map(|_| Vec::with_capacity(c.len() * 2 / LP + 8)).collect();
            for (k, &(a, b)) in c.iter().enumerate() {
                let slot = (2 * (eoff[ci] + k)) as u32;
                out[part(a)].push((a, slot));
                out[part(b)].push((b, slot + 1));
            }
            out
        })
        .collect();
    // per partition: intern -> (distinct ids, (slot, local index) pairs)
    let interned: Vec<(Vec<S>, Vec<(u32, u32)>)> = (0..LP)
        .into_par_iter()
        .map(|p| {
            let n: usize = scattered.iter().map(|v| v[p].len()).sum();
            let mut m: PMap<u32> = PMap::with_capacity(n);
            let mut ids: Vec<S> = Vec::with_capacity(n);
            let mut slots: Vec<(u32, u32)> = Vec::with_capacity(n);
            for v in &scattered {
                for &(s, slot) in &v[p] {
                    let l = *m.get_or_insert_with(s, || {
                        ids.push(s);
                        (ids.len() - 1) as u32
                    });
                    slots.push((slot, l));
                }
            }
            (ids, slots)
        })
        .collect();
    drop(scattered);
    let mut poff = Vec::with_capacity(LP + 1);
    poff.push(0usize);
    for (ids, _) in &interned {
        poff.push(poff.last().unwrap() + ids.len());
    }
    let n = poff[LP];
    let mut side = vec![0u32; 2 * n_edges];
    let mut ids: Vec<S> = Vec::with_capacity(n);
    for (p, (pids, slots)) in interned.iter().enumerate() {
        ids.extend_from_slice(pids);
        let o = poff[p] as u32;
        for &(slot, l) in slots {
            side[slot as usize] = o + l;
        }
    }
    drop(interned);
    st.mark("links: intern");
    let mut parent: Vec<u32> = (0..n as u32).collect();
    let mut size: Vec<u32> = vec![1; n];
    fn find(parent: &mut [u32], mut x: u32) -> u32 {
        while parent[x as usize] != x {
            let g = parent[parent[x as usize] as usize];
            parent[x as usize] = g;
            x = g;
        }
        x
    }
    for e in side.chunks_exact(2) {
        let (ra, rb) = (find(&mut parent, e[0]), find(&mut parent, e[1]));
        if ra != rb {
            let (big, small) = if size[ra as usize] >= size[rb as usize] { (ra, rb) } else { (rb, ra) };
            parent[small as usize] = big;
            size[big as usize] += size[small as usize];
        }
    }
    drop(side);
    st.mark("links: union");
    let mut name: Vec<u32> = (0..n as u32).collect(); // per root: index of the smallest member
    let mut root = vec![0u32; n];
    for i in 0..n {
        let r = find(&mut parent, i as u32);
        root[i] = r;
        if ids[i] < ids[name[r as usize] as usize] {
            name[r as usize] = i as u32;
        }
    }
    let mut multi = 0u64;
    let mut largest = 0u64;
    for i in 0..n {
        if root[i] as usize == i {
            if size[i] > 1 {
                multi += 1;
            }
            largest = largest.max(size[i] as u64);
        }
    }
    st.mark("links: names");
    // name map, one table per key partition (RW1: u64 keys, flat tables)
    let pairs: Vec<Vec<(u64, u64)>> = (0..LP)
        .into_par_iter()
        .map(|p| {
            let mut v = Vec::new();
            for i in poff[p]..poff[p + 1] {
                let nm = ids[name[root[i] as usize] as usize];
                if nm != ids[i] {
                    v.push((key_of(ids[i]), key_of(nm)));
                }
            }
            v
        })
        .collect();
    let mut by_part: Vec<Vec<(u64, u64)>> = (0..LP).map(|_| Vec::new()).collect();
    for v in pairs {
        for (k, nk) in v {
            by_part[(packed::fmix(k) >> 40) as usize & (LP - 1)].push((k, nk));
        }
    }
    let parts: Vec<KeyMap> = by_part
        .into_par_iter()
        .map(|v| {
            let mut m = KeyMap::with_capacity(v.len());
            for (k, nk) in v {
                m.insert_first(k, nk);
            }
            m
        })
        .collect();
    st.mark("links: out map");
    stats.add(K::LkIds, n as u64);
    stats.add(K::LkPeopleMultiId, multi);
    stats.add(K::LkLargestPerson, largest);
    Names { parts }
}

// ====================================================================== clicks

/// RW1: a click copy as scanned. `person` is EMPTY when the copy is invalid (empty user,
/// bad or out-of-window ts, unknown campaign); `why` then says which (for stats only).
#[derive(Clone, Copy)]
struct ClRec {
    cid: u64,
    person: u64,
    ts: i64,
    camp: u32,
    why: u32,
}

/// A valid, deduplicated click, partitioned by person.
#[derive(Clone, Copy)]
pub struct ClickV {
    pub person: u64,
    pub ts: i64,
    pub cid: u64,
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

/// Dedup table over the copies of one partition: key -> location (source, position) of
/// the kept copy. `better(new, kept)` decides replacement; first copy wins on ties.
fn dedup<T: Copy>(parts: &[Vec<T>], key: impl Fn(&T) -> u64, better: impl Fn(&T, &T) -> bool) -> Vec<T> {
    let n: usize = parts.iter().map(Vec::len).sum();
    let cap = (n * 2).next_power_of_two().max(16);
    let mask = cap - 1;
    let mut slots: Vec<(u64, u64)> = vec![(keys::EMPTY, 0); cap];
    let mut used = 0usize;
    for (si, part) in parts.iter().enumerate() {
        for (pi, r) in part.iter().enumerate() {
            let k = key(r);
            let mut i = packed::fmix(k) as usize & mask;
            loop {
                let s = &mut slots[i];
                if s.0 == keys::EMPTY {
                    *s = (k, ((si as u64) << 32) | pi as u64);
                    used += 1;
                    break;
                }
                if s.0 == k {
                    let kept = &parts[(s.1 >> 32) as usize][(s.1 & 0xffff_ffff) as usize];
                    if better(r, kept) {
                        s.1 = ((si as u64) << 32) | pi as u64;
                    }
                    break;
                }
                i = (i + 1) & mask;
            }
        }
    }
    let mut out = Vec::with_capacity(used);
    for s in &slots {
        if s.0 != keys::EMPTY {
            out.push(parts[(s.1 >> 32) as usize][(s.1 & 0xffff_ffff) as usize]);
        }
    }
    out
}

fn clicks(src: &[&'static Source], campaigns: &PMap<u32>, lo: i64, hi: i64, names: &Names, stats: &Stats) -> ByPerson<ClickV> {
    let mut st = Stage::start();
    let items = work_items::<4>(src, ["click_id", "user_id", "campaign_id", "ts"]);
    // scan + validate each copy + partition on click_id
    let scattered: Vec<Vec<Vec<ClRec>>> = items
        .par_iter()
        .map(|(order, chunk)| {
            let mut parts: Vec<Vec<ClRec>> = (0..NB).map(|_| Vec::with_capacity(chunk.len() / 48 / NB + 8)).collect();
            let (mut lines, mut bad, mut noid) = (0u64, 0u64, 0u64);
            let mut pend: Pending<ClRec> = Pending::new();
            let mut put = |mut r: ClRec, person: u64| {
                r.person = person;
                parts[keys::part(r.cid, LOG_NB)].push(r);
            };
            for_each_record::<4>(chunk, |rec| {
                lines += 1;
                let Some(f) = rec else {
                    bad += 1;
                    return;
                };
                let f = reorder(f, order);
                if f[0].is_empty() {
                    noid += 1;
                    return;
                }
                let cid = key_of(f[0]);
                let mut r = ClRec { cid, person: keys::EMPTY, ts: 0, camp: 0, why: 0 };
                if f[1].is_empty() {
                    r.why = 1;
                } else {
                    let ts = timefmt::parse_in(f[3], lo, hi);
                    if ts == BAD {
                        r.why = 2;
                    } else {
                        match campaigns.get(f[2]) {
                            None => r.why = 3,
                            Some(&c) => {
                                r.ts = ts;
                                r.camp = c;
                                pend.push(names, r, key_of(f[1]), &mut put);
                                return;
                            }
                        }
                    }
                }
                put(r, keys::EMPTY);
            });
            pend.flush(names, &mut put);
            drop(put);
            stats.add(K::ClLines, lines);
            stats.add(K::ClMalformed, bad);
            stats.add(K::ClNoId, noid);
            parts
        })
        .collect();
    st.mark("clicks: scan+parse+partition");
    let by_cid = transpose(scattered);
    st.mark("clicks: transpose");
    // rules.md, Clicks step 2: copies are identical, keep any one (the first in scan
    // order: transpose keeps file-then-chunk order, so this is thread-count independent)
    let out: Vec<Vec<Vec<ClickV>>> = by_cid
        .into_par_iter()
        .map(|parts| {
            let n: usize = parts.iter().map(Vec::len).sum();
            let kept = dedup(&parts, |r: &ClRec| r.cid, |_, _| false);
            stats.add(K::ClDupCopies, (n - kept.len()) as u64);
            let mut dst: Vec<Vec<ClickV>> = (0..NB).map(|_| Vec::with_capacity(kept.len() / NB + 8)).collect();
            let mut why = [0u64; 4];
            for r in &kept {
                if r.person == keys::EMPTY {
                    why[r.why as usize] += 1;
                } else {
                    dst[keys::part(r.person, LOG_NB)].push(ClickV { person: r.person, ts: r.ts, cid: r.cid, camp: r.camp });
                }
            }
            stats.add(K::ClBadUser, why[1]);
            stats.add(K::ClBadTs, why[2]);
            stats.add(K::ClBadCampaign, why[3]);
            stats.add(K::ClSurvived, dst.iter().map(|v| v.len() as u64).sum());
            dst
        })
        .collect();
    st.mark("clicks: dedup+validate+repartition");
    let t = transpose(out);
    st.mark("clicks: transpose");
    t
}

// ====================================================================== events

/// RW1: an event copy as scanned (40 bytes). Validity is decided per copy at scan time
/// (rules.md validates the kept copy, and the kept copy is one of these, so deciding
/// early is equivalent); dedup itself still runs over every copy, valid or not.
/// `person` is EMPTY for an invalid copy, and `price` then holds the reason (stats only).
#[derive(Clone, Copy)]
struct EvRec {
    eid: u64,
    rank: u64, // order-preserving ingest_ts; u64::MAX if unparseable (ranks last)
    person: u64,
    ts: i64,
    price: u32, // > 0 iff a valid purchase
    cat: u32,
}

/// A valid, deduplicated event, partitioned by person (24 bytes).
#[derive(Clone, Copy)]
pub struct EvV {
    pub person: u64,
    pub ts: i64,
    pub cat: u32,
    pub price: u32,
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

/// Dedup rank: smaller is preferred. Order-preserving map of i64 to u64; an unparseable
/// ingest_ts ranks after every parseable one. (A parseable i64::MAX and an unparseable
/// one share u64::MAX; both copies are dropped as late or invalid, so the tie is moot.)
#[inline]
fn rank_of(ingest: Option<i64>) -> u64 {
    match ingest {
        Some(v) => (v as u64) ^ (1 << 63),
        None => u64::MAX,
    }
}

#[allow(clippy::too_many_arguments)]
fn events(src: &[&'static Source], products: &PMap<u32>, unknown_cat: u32, lo: i64, hi: i64, names: &Names, stats: &Stats) -> ByPerson<EvV> {
    const NAMES: [&str; 7] = ["event_id", "user_id", "ts", "ingest_ts", "event_type", "product_id", "price_cents"];
    let mut st = Stage::start();
    let items = work_items::<7>(src, NAMES);
    let scattered: Vec<Vec<Vec<EvRec>>> = items
        .par_iter()
        .map(|(order, chunk)| {
            let mut parts: Vec<Vec<EvRec>> = (0..NB).map(|_| Vec::with_capacity(chunk.len() / 80 / NB + 8)).collect();
            let (mut lines, mut bad, mut noid) = (0u64, 0u64, 0u64);
            let mut pend: Pending<EvRec> = Pending::new();
            let mut put = |mut r: EvRec, person: u64| {
                r.person = person;
                parts[keys::part(r.eid, LOG_NB)].push(r);
            };
            for_each_record::<7>(chunk, |rec| {
                lines += 1;
                let Some(f) = rec else {
                    bad += 1;
                    return;
                };
                let f = reorder(f, order);
                if f[0].is_empty() {
                    noid += 1;
                    return;
                }
                let eid = key_of(f[0]);
                let ingest = parse_ingest(f[3]);
                let mut r = EvRec { eid, rank: rank_of(ingest), person: keys::EMPTY, ts: 0, price: 0, cat: 0 };
                // validation of this copy, in the order of rules.md (reason codes for stats)
                let typ = match f[4] {
                    b"view" => T_VIEW,
                    b"cart" => T_CART,
                    b"purchase" => T_PURCHASE,
                    _ => T_BAD,
                };
                let ts = if f[1].is_empty() { BAD } else { timefmt::parse_in(f[2], lo, hi) };
                if f[1].is_empty() {
                    r.price = 0;
                } else if ts == BAD {
                    r.price = 1;
                } else if typ == T_BAD {
                    r.price = 2;
                } else if f[5].is_empty() {
                    r.price = 3;
                } else {
                    let price = if typ == T_PURCHASE { parse_price(f[6]) } else { 0 };
                    if typ == T_PURCHASE && price == 0 {
                        r.price = 4;
                    } else {
                        match ingest {
                            Some(ing) if ing.saturating_sub(ts) <= DAY => {
                                r.ts = ts;
                                r.price = price;
                                r.cat = products.get(f[5]).copied().unwrap_or(unknown_cat);
                                // RW2: person resolved in a prefetched batch
                                pend.push(names, r, key_of(f[1]), &mut put);
                                return;
                            }
                            _ => r.price = 5,
                        }
                    }
                }
                put(r, keys::EMPTY);
            });
            pend.flush(names, &mut put);
            drop(put);
            stats.add(K::EvLines, lines);
            stats.add(K::EvMalformed, bad);
            stats.add(K::EvNoId, noid);
            parts
        })
        .collect();
    st.mark("events: scan+parse+partition");
    let by_eid = transpose(scattered);
    st.mark("events: transpose");
    let out: Vec<Vec<Vec<EvV>>> = by_eid
        .into_par_iter()
        .map(|parts| {
            let n: usize = parts.iter().map(Vec::len).sum();
            // rules.md, Events step 2: keep the copy with the smallest ingest_ts; A2: on a
            // tie the first copy stays (equal-ingest copies are promised identical)
            let kept = dedup(&parts, |r: &EvRec| r.eid, |a, b| a.rank < b.rank);
            stats.add(K::EvDupCopies, (n - kept.len()) as u64);
            let mut dst: Vec<Vec<EvV>> = (0..NB).map(|_| Vec::with_capacity(kept.len() / NB + 8)).collect();
            let mut c = [0u64; 8];
            for r in &kept {
                if r.person == keys::EMPTY {
                    c[r.price as usize] += 1;
                    continue;
                }
                if r.cat == unknown_cat {
                    c[6] += 1;
                }
                dst[keys::part(r.person, LOG_NB)].push(EvV { person: r.person, ts: r.ts, cat: r.cat, price: r.price });
            }
            for (k, i) in [(K::EvBadUser, 0), (K::EvBadTs, 1), (K::EvBadType, 2), (K::EvBadProduct, 3), (K::EvBadPrice, 4), (K::EvLate, 5), (K::EvUnknownProduct, 6)] {
                stats.add(k, c[i]);
            }
            stats.add(K::EvKeptBad, c[..5].iter().sum());
            stats.add(K::EvSurvived, dst.iter().map(|v| v.len() as u64).sum());
            dst
        })
        .collect();
    st.mark("events: dedup+validate+repartition");
    let t = transpose(out);
    st.mark("events: transpose");
    t
}

const T_VIEW: u8 = 0;
const T_CART: u8 = 1;
const T_PURCHASE: u8 = 2;
const T_BAD: u8 = 3;

// ====================================================================== sessions

fn sessionize(
    b: usize,
    evs: &ByPerson<EvV>,
    cls: &ByPerson<ClickV>,
    ncamp: usize,
    day0: i32,
    ndays: usize,
    camp: &mut [[u64; 4]],
    stats: &Stats,
) -> Vec<output::UserRow> {
    // RW1: persons are u64 keys; sort events by (person, ts) directly
    let n: usize = evs[b].iter().map(Vec::len).sum();
    let mut ev: Vec<EvV> = Vec::with_capacity(n);
    for part in &evs[b] {
        ev.extend_from_slice(part);
    }
    ev.sort_unstable_by_key(|e| (e.person, e.ts));
    // clicks sorted by (person, ts asc, click_id desc in byte order), so the last click
    // of a window is the latest one with the byte-wise smallest click_id
    let nc: usize = cls[b].iter().map(Vec::len).sum();
    let mut cl: Vec<ClickV> = Vec::with_capacity(nc);
    for part in &cls[b] {
        cl.extend_from_slice(part);
    }
    cl.sort_unstable_by(|x, y| (x.person, x.ts).cmp(&(y.person, y.ts)).then_with(|| keys::cmp_bytes(y.cid, x.cid)));

    let mut rows: Vec<output::UserRow> = Vec::with_capacity(n / 8 + 16);
    // sliding-window distinct-campaign set: count per campaign + list of active ones
    let mut cnt = vec![0u32; ncamp];
    let mut pos = vec![u32::MAX; ncamp];
    let mut active: Vec<u32> = Vec::new();
    let (mut sessions_total, mut credited, mut assists, mut persons) = (0u64, 0u64, 0u64, 0u64);

    let mut i = 0usize; // event cursor
    let mut j = 0usize; // click cursor (first click of current person)
    let mut cat_rev: Vec<(u32, u64)> = Vec::new();
    while i < ev.len() {
        let p = ev[i].person;
        persons += 1;
        let mut pe = i + 1;
        while pe < ev.len() && ev[pe].person == p {
            pe += 1;
        }
        while j < cl.len() && cl[j].person < p {
            j += 1;
        }
        let mut pc = j;
        while pc < cl.len() && cl[pc].person == p {
            pc += 1;
        }
        let (mut lo, mut hi) = (j, j); // window [lo, hi) of this person's clicks

        let mut day_cur: i32 = i32::MIN;
        let (mut d_sessions, mut d_events, mut d_rev) = (0u64, 0u64, 0u64);
        cat_rev.clear();

        let mut k = i;
        while k < pe {
            // one session: [k, e); equal timestamps never split (gap 0)
            let start = ev[k].ts;
            let mut e = k + 1;
            while e < pe && ev[e].ts - ev[e - 1].ts <= GAP {
                e += 1;
            }
            let day = start.div_euclid(DAY) as i32;
            let (mut rev, mut has_p) = (0u64, false);
            for x in &ev[k..e] {
                if x.price > 0 {
                    has_p = true;
                    rev += x.price as u64;
                }
            }
            // attribution window: clicks with start - 7d <= ts <= start
            while hi < pc && cl[hi].ts <= start {
                let c = cl[hi].camp as usize;
                if cnt[c] == 0 {
                    pos[c] = active.len() as u32;
                    active.push(c as u32);
                }
                cnt[c] += 1;
                hi += 1;
            }
            while lo < hi && cl[lo].ts < start - WEEK {
                let c = cl[lo].camp as usize;
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
            let credit = if hi > lo { cl[hi - 1].camp } else { ORGANIC };
            // session starts lie in [ws, we), so 0 <= day - day0 < ndays
            let doff = (day - day0) as usize;
            debug_assert!(doff < ndays);
            let ci = if credit == ORGANIC { ncamp } else { credit as usize };
            let a = &mut camp[ci * ndays + doff];
            a[0] += 1;
            a[1] += has_p as u64;
            a[2] += rev;
            for &c in &active {
                camp[c as usize * ndays + doff][3] += 1;
            }
            sessions_total += 1;
            credited += (credit != ORGANIC) as u64;
            assists += active.len() as u64;

            // user_daily accumulation
            if day != day_cur {
                if day_cur != i32::MIN {
                    rows.push(output::UserRow::new(p, day_cur, d_sessions, d_events, d_rev, &cat_rev));
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
                    if x.price > 0 {
                        match cat_rev.iter_mut().find(|c| c.0 == x.cat) {
                            Some(c) => c.1 += x.price as u64,
                            None => cat_rev.push((x.cat, x.price as u64)),
                        }
                    }
                }
            }
            k = e;
        }
        rows.push(output::UserRow::new(p, day_cur, d_sessions, d_events, d_rev, &cat_rev));
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
    stats.add(K::Persons, persons);
    rows
}

#[cfg(test)]
mod layout {
    /// Record sizes drive memory traffic in the scan/partition phase (P0 profile).
    #[test]
    fn record_sizes() {
        eprintln!("EvRec={} EvV={} ClRec={} ClickV={}", std::mem::size_of::<super::EvRec>(),
            std::mem::size_of::<super::EvV>(), std::mem::size_of::<super::ClRec>(), std::mem::size_of::<super::ClickV>());
        assert_eq!(std::mem::size_of::<super::EvRec>(), 40);
        assert_eq!(std::mem::size_of::<super::EvV>(), 24);
    }
}

#[cfg(test)]
mod packed_keys {
    use super::*;
    // pack9 order/domain tests: packed.rs
    /// Night-03: partition bits spread for packed IDs.
    #[test]
    fn packed_partition_spread() {
        let st = RandomState(PART_SEED);
        let mut b = [0u32; NB];
        for i in 0..512_000u32 {
            b[part_of_u64(&st, pack9(format!("e{i:x}").as_bytes()).unwrap())] += 1;
        }
        let (mn, mx) = (*b.iter().min().unwrap(), *b.iter().max().unwrap());
        let avg = 512_000 / NB as u32;
        assert!(mn > avg * 8 / 10 && mx < avg * 12 / 10, "{mn} {mx}");
    }
}

#[cfg(test)]
mod link_parts {
    use std::hash::BuildHasher;
    /// Night-02: link partitions (LINK_SEED, bits 40..46) are well spread over user-like IDs.
    #[test]
    fn link_partition_spread() {
        let st = super::RandomState(super::LINK_SEED);
        let mut b = [0u32; super::LP];
        for i in 0..640_000u32 {
            b[(crate::packed::id_hash(&st, format!("u{i}").as_bytes()) >> 40) as usize & (super::LP - 1)] += 1;
        }
        let (mn, mx) = (*b.iter().min().unwrap(), *b.iter().max().unwrap());
        assert!(mn > 9_500 && mx < 10_500, "{mn} {mx}");
    }
}
