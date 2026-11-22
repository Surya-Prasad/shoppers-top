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

#[allow(dead_code)]
mod alloc;
mod dialect;
mod fast;
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

// RW8's huge-page allocator (src/alloc.rs) measured neutral on x86 CI and adds unsafe code
// whose failure would score zero; RW11 leaves it unregistered (the system allocator is used).
use rayon::prelude::*;

use dialect::{Source, chunks, for_each_record_bq, for_each_record_idx, for_each_record_ix, for_each_record_swar, csv_files, data_region, for_each_line, for_each_record, header_order, split};
use keys::{KeyMap, key_of};
use packed::{IdMap, pack9};
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

/// Fields in the documented order. RW24: the identity order (every generated file) is a
/// no-op, checked with one predictable branch instead of an indexed copy per line.
#[inline(always)]
fn reorder<const N: usize>(f: [S; N], order: &[usize; N]) -> [S; N] {
    if is_identity(order) {
        return f;
    }
    let mut out = f;
    for i in 0..N {
        out[i] = f[order[i]];
    }
    out
}

#[inline(always)]
fn is_identity<const N: usize>(order: &[usize; N]) -> bool {
    let mut i = 0;
    while i < N {
        if order[i] != i {
            return false;
        }
        i += 1;
    }
    true
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
    // RW31: links resolve while clicks and events are scanned (the scans carry user keys;
    // persons are resolved in the dedup steps below)
    // v2: links first, on all threads. Run concurrently (RW31), their tasks were starved by
    // the scans and finished last at 60M (x86 CI: links 3.8 s vs events scan 3.0 s);
    // alone they take a fraction of that.
    // v2: links resolve on a small pool of their own while the events scan (which carries
    // user keys; persons are resolved in the events dedup) runs on the main pool. Sharing one
    // pool starved the link tasks (RW31); running links first serialized their sequential
    // steps. Clicks are scanned once links are ready (they are partitioned by person there).
    let link_threads = (ncpu / 2).max(1);
    let (names, event_copies) = std::thread::scope(|sc| {
        let h = sc.spawn(|| {
            let pool = rayon::ThreadPoolBuilder::new().num_threads(link_threads).build().expect("link pool");
            pool.install(|| resolve_links(lk_src, &stats))
        });
        let e = events_scan(ev_src, &products, unknown_cat, ws, we, None, &stats);
        (h.join().expect("links thread"), e)
    });
    let click_copies = clicks_scan(cl_src, &campaigns, ws - WEEK, we, &names, &stats);
    lap("links resolved + clicks/events scanned");
    if std::env::var("SS_EXP2").is_ok() {
        experiment2(ev_src, &products, ws, we);
        return;
    }
    if std::env::var("SS_EXP").is_ok() {
        experiment(ev_src, &products, ws, we, &names);
        return;
    }

    // v2: nothing reads the events/clicks/links mappings after the scans (records hold keys;
    // interned IDs own copies; products/campaigns stay mapped for output names), so they
    // are unmapped in the background instead of during process teardown.
    let unmapper = {
        let spans: Vec<(usize, usize)> = all[2..].iter().map(|s| (s.bytes().as_ptr() as usize, s.bytes().len())).collect();
        std::thread::spawn(move || {
            for (p, l) in spans {
                if l > 0 {
                    // SAFETY: each span is a whole leaked mapping that is never read again
                    unsafe { libc::munmap(p as *mut libc::c_void, l) };
                }
            }
        })
    };

    // ---------------------------------------------------------------- clicks
    let clicks_by_person = click_copies; // v2 P2: no click dedup (see clicks_scan_items)
    lap("clicks");

    // ---------------------------------------------------------------- events
    // v2: the compact 16-byte event layout when every value fits it, else the 24-byte one
    let ncamp = camp_ids.len();
    let compact = fast_enabled() && we - ws <= u32::MAX as i64 && cat_names.len() <= 256;
    let ctx = SessCtx { ncamp, ws, we, stats: &stats, unknown_cat, names: &names };
    let (user_parts, camp_agg, freer) = if compact {
        sessions_phase::<EvC>(event_copies, clicks_by_person, &ctx, &lap)
    } else {
        sessions_phase::<EvV>(event_copies, clicks_by_person, &ctx, &lap)
    };

    // ---------------------------------------------------------------- output
    output::write_all(&out, user_parts, camp_agg, &cat_names, &camp_ids, &channels, ncpu);
    lap("outputs written");
    let _ = freer.join();

    let json = stats.json();
    eprint!("stats {json}");
    if let Ok(p) = std::env::var("STATS_FILE") {
        let _ = std::fs::write(p, &json);
    }
    // cheap self-check from rules.md: assists >= credited non-organic sessions
    assert!(stats.get(K::AssistPairs) >= stats.get(K::CreditedSessions), "assist invariant violated");
    let _ = unmapper.join();
    lap("done");
    // v2 P5: the outputs are complete and renamed into place; skip the destructors of the
    // remaining tables (the kernel reclaims everything at exit anyway)
    std::process::exit(0);
}

/// Shared inputs of the sessions phase.
struct SessCtx<'a> {
    ncamp: usize,
    ws: i64,
    we: i64,
    stats: &'a Stats,
    unknown_cat: u32,
    names: &'a Names,
}

/// v2: events dedup + sessions + campaign/day aggregation, generic over the event layout.
#[allow(clippy::type_complexity)]
fn sessions_phase<V: EvLike>(
    event_copies: ByPerson<EvRec>,
    clicks_by_person: ByPerson<ClickV>,
    cx: &SessCtx,
    lap: &dyn Fn(&str),
) -> (Vec<Vec<output::UserRow>>, Map<(u32, i32), [u64; 4]>, std::thread::JoinHandle<()>) {
    let (ncamp, ws, we, stats, unknown_cat) = (cx.ncamp, cx.ws, cx.we, cx.stats, cx.unknown_cat);
    let events_by_person: ByPerson<V> = events_dedup(event_copies, unknown_cat, Some(cx.names), ws, stats);
    lap("events");

    // ---------------------------------------------------------------- sessions
    // Night-01: campaign/day accumulators are a dense array per rayon fold (campaign
    // index, ORGANIC last; day offset from the first window day) instead of a hash map.
    let day0 = ws.div_euclid(DAY) as i32;
    let ndays = ((we - 1).div_euclid(DAY) as i32 - day0 + 1).max(1) as usize;
    let dense_len = (ncamp + 1) * ndays;
    // RW17: credited counters ([sessions, purchasing, revenue]) and assists (u32) live in
    // separate per-fold arrays: ~104M assist increments at 60M hit a ~112 KB array that
    // stays in L2 instead of a ~900 KB array of 32-byte cells.
    type Acc = (Vec<[u64; 3]>, Vec<u32>);
    let folded: Vec<(Vec<Vec<output::UserRow>>, Acc)> = (0..NB)
        .into_par_iter()
        .fold(
            || (Vec::new(), (Vec::new(), Vec::new())),
            |(mut rows, mut acc): (Vec<Vec<output::UserRow>>, Acc), b| {
                if acc.0.is_empty() {
                    acc = (vec![[0u64; 3]; dense_len], vec![0u32; dense_len]);
                }
                rows.push(sessionize(b, &events_by_person, &clicks_by_person, ncamp, day0, ndays, &mut acc.0, &mut acc.1, stats, ws));
                (rows, acc)
            },
        )
        .collect();
    let mut dense = vec![[0u64; 4]; dense_len];
    let mut user_parts = Vec::with_capacity(NB);
    for (rows, (cred, assist)) in folded {
        user_parts.extend(rows);
        for ((d, c), a) in dense.iter_mut().zip(cred.iter()).zip(assist.iter()) {
            d[0] += c[0];
            d[1] += c[1];
            d[2] += c[2];
            d[3] += *a as u64;
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

    // RW30: returning gigabytes of event/click vectors to the OS takes ~0.06 s on one
    // thread; do it in the background while the output files are written
    let freer = std::thread::spawn(move || {
        drop(events_by_person);
        drop(clicks_by_person);
    });
    (user_parts, camp_agg, freer)
}

/// SS_EXP2=1: single-thread cost of the v2 fast-path layers (profiling only).
fn experiment2(src: &[&'static Source], products: &IdMap, lo: i64, hi: i64) {
    const NAMES: [&str; 7] = ["event_id", "user_id", "ts", "ingest_ts", "event_type", "product_id", "price_cents"];
    let items = work_items::<7>(src, NAMES);
    let mut lines = 0u64;
    for (_, c) in &items {
        for_each_line(c, |_| lines += 1);
    }
    let layer = |name: &str, f: &mut dyn FnMut(*const u8) -> Option<usize>| {
        let t = Instant::now();
        for (_, c) in &items {
            fast::for_each_line_fast(c, &mut *f, |_| {});
        }
        eprintln!("{name:<28} {:>6.1} ns/line", t.elapsed().as_secs_f64() * 1e9 / lines as f64);
    };
    let mut acc = 0u64;
    {
        // which lines the fast decoder declines, by first failing step
        let mut why = [0u64; 8];
        for (_, c) in &items {
            fast::for_each_line_fast(c, |p| unsafe { fast::event(p, lo, hi, b'p') }.map(|(l, _)| l), |line| {
                let w = match split::<7>(line) {
                    None => 0,
                    Some(f) if f.iter().any(|x| x.is_empty() && !std::ptr::eq(*x, f[6])) => 1,
                    Some(f) => match timefmt::parse(f[2]) { None => 2, Some(t) if t < lo || t >= hi => 3, _ => if timefmt::parse(f[3]).is_none() { 4 } else if !matches!(f[4], b"view" | b"cart" | b"purchase") { 5 } else if f[4] == b"purchase" && parse_price(f[6]) == 0 { 6 } else { 7 } },
                };
                why[w] += 1;
            });
        }
        eprintln!("declined: malformed {} empty-field {} bad-ts {} out-of-window {} bad-ingest {} bad-type {} bad-price {} other(late/shape) {} of {lines}", why[0], why[1], why[2], why[3], why[4], why[5], why[6], why[7]);
    }
    layer("fields only (cold)", &mut |p| unsafe { fast::fields::<7>(p) }.map(|(l, f)| { acc ^= f[3].0 as u64; l }));
    layer("fields only", &mut |p| unsafe { fast::fields::<7>(p) }.map(|(l, f)| { acc ^= f[3].0 as u64; l }));
    layer("event decode", &mut |p| unsafe { fast::event(p, lo, hi, b'p') }.map(|(l, e)| { acc ^= e.eid ^ e.ts as u64; l }));
    layer("+ product lookup", &mut |p| unsafe { fast::event(p, lo, hi, b'p') }.map(|(l, e)| { acc ^= products.get_packed(packed::fmix(e.product), e.product).unwrap_or(0) as u64; l }));
    let mut parts: Vec<Vec<EvRec>> = (0..NB).map(|_| Vec::with_capacity(lines as usize / NB + 1024)).collect();
    layer("+ push (preallocated)", &mut |p| unsafe { fast::event(p, lo, hi, b'p') }.map(|(l, e)| {
        parts[keys::part(e.eid, LOG_NB)].push(EvRec { eid: e.eid, rank: e.ingest as u64, person: e.user, ts: e.ts, price: e.price, cat: 0 }); l }));
    eprintln!("acc {acc} {}", parts.iter().map(Vec::len).sum::<usize>());
}

/// SS_EXP=1: single-thread cost of each scan layer over all event files (profiling only).
fn experiment(src: &[&'static Source], products: &IdMap, lo: i64, hi: i64, names: &Names) {
    const NAMES: [&str; 7] = ["event_id", "user_id", "ts", "ingest_ts", "event_type", "product_id", "price_cents"];
    let items = work_items::<7>(src, NAMES);
    let bytes: usize = items.iter().map(|(_, c)| c.len()).sum();
    let mut lines = 0u64;
    for (_, c) in &items {
        for_each_line(c, |_| lines += 1);
    }
    if let Ok(which) = std::env::var("SS_ONLY") {
        // one scanner, 5 passes, nothing else (for instruction/cycle counts)
        let t = Instant::now();
        let mut acc = 0u64;
        for _ in 0..5 {
            for (_, c) in &items {
                let mut g = |rec: Option<[S; 7]>| {
                    if let Some(r) = rec {
                        acc = acc.wrapping_add(r[0].len() as u64 + r[6].len() as u64);
                    }
                };
                match which.as_str() {
                    "v1" => for_each_record::<7>(c, &mut g),
                    "swar" => for_each_record_swar::<7>(c, &mut g),
                    "ix" => for_each_record_ix::<7>(c, &mut g),
                    "bq" => for_each_record_bq::<7>(c, &mut g),
                    "lines" => for_each_line(c, |l| acc = acc.wrapping_add(l.len() as u64)),
                    "ix1" => acc = acc.wrapping_add(dialect::ix_stage1(c) as u64),
                    "masks" => acc = acc.wrapping_add(dialect::masks_only(c)),
                    _ => for_each_record_idx::<7>(c, &mut g),
                }
            }
        }
        let secs = t.elapsed().as_secs_f64();
        eprintln!("SS_ONLY={which}: {:.1} ns/line ({acc})", secs * 1e9 / (5.0 * lines as f64));
        return;
    }
    let run = |name: &str, f: &dyn Fn(&[S; 7]) -> u64| {
        let t = Instant::now();
        let mut acc = 0u64;
        for (order, c) in &items {
            for_each_record_bq::<7>(c, |rec| {
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
    run("records (RW16 production)", &|f| f[0].len() as u64);
    {
        let t = Instant::now();
        let mut acc = 0u64;
        for (_, c) in &items {
            for_each_record_idx::<7>(c, |rec| {
                if let Some(r) = rec {
                    acc = acc.wrapping_add(r[0].len() as u64);
                }
            });
        }
        let secs = t.elapsed().as_secs_f64();
        eprintln!("{:<28} {secs:>6.3}s  {:>6.1} ns/line  (acc {acc})", "records (RW4 struct index)", secs * 1e9 / lines as f64);
        let t = Instant::now();
        let mut acc = 0u64;
        for (_, c) in &items {
            for_each_record::<7>(c, |rec| {
                if let Some(r) = rec {
                    acc = acc.wrapping_add(r[0].len() as u64);
                }
            });
        }
        let secs = t.elapsed().as_secs_f64();
        eprintln!("{:<28} {secs:>6.3}s  {:>6.1} ns/line  (acc {acc})", "records (night-04, direct)", secs * 1e9 / lines as f64);
        let t = Instant::now();
        let mut acc = 0u64;
        for (_, c) in &items {
            for_each_record_swar::<7>(c, |rec| {
                if let Some(r) = rec {
                    acc = acc.wrapping_add(r[0].len() as u64);
                }
            });
        }
        let secs = t.elapsed().as_secs_f64();
        eprintln!("{:<28} {secs:>6.3}s  {:>6.1} ns/line  (acc {acc})", "records (RW9 memchr+SWAR)", secs * 1e9 / lines as f64);
        let t = Instant::now();
        let mut acc = 0u64;
        for (_, c) in &items {
            for_each_record_ix::<7>(c, |rec| {
                if let Some(r) = rec {
                    acc = acc.wrapping_add(r[0].len() as u64);
                }
            });
        }
        let secs = t.elapsed().as_secs_f64();
        eprintln!("{:<28} {secs:>6.3}s  {:>6.1} ns/line  (acc {acc})", "records (RW10 two-stage)", secs * 1e9 / lines as f64);
    }
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
            ^ (f[4] == b"purchase") as u64 ^ parse_price(f[6]) as u64 ^ products.get(f[5]).unwrap_or(0) as u64
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
                    price: parse_price(f[6]), cat: products.get(f[5]).unwrap_or(0) };
                parts[keys::part(eid, LOG_NB)].push(r);
            }
        });
        kept += parts.iter().map(Vec::len).sum::<usize>();
    }
    let secs = t.elapsed().as_secs_f64();
    eprintln!("{:<28} {secs:>6.3}s  {:>6.1} ns/line  (kept {kept})", "+ person + scatter", secs * 1e9 / lines as f64);
    run("+ product lookup", &|f| {
        key_of(f[0]) ^ key_of(f[1]) ^ timefmt::parse_in(f[2], lo, hi) as u64 ^ parse_ingest(f[3]).unwrap_or(0) as u64
            ^ (f[4] == b"purchase") as u64 ^ parse_price(f[6]) as u64 ^ products.get(f[5]).unwrap_or(0) as u64
    });
}

// ====================================================================== reference tables

/// products(product_id PK, category). First occurrence wins if a key repeats.
/// Categories are numbered in byte order, so "smallest index" == "byte-wise smallest name".
fn load_products(src: &'static Source) -> (IdMap, Vec<S>) {
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
    let mut products: IdMap = IdMap::with_dense(rows.len(), b'p'); // v2: `p<number>` IDs by index
    for (p, c) in rows {
        products.insert_first(p, idx[c]);
    }
    (products, cats)
}

/// campaigns(campaign_id PK, channel). Returns id -> dense index, channel per index, id per index.
fn load_campaigns(src: &'static Source) -> (IdMap, Vec<S>, Vec<S>) {
    let bytes: S = src.bytes();
    let order = header_order::<2>(bytes, ["campaign_id", "channel"]);
    let mut m: IdMap = IdMap::with_capacity(4096);
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
/// RW31: person resolution in dedup sinks, batched with prefetch: rows carry the user key
/// in `person` until here; `emit(row, person)` gets the resolved person.
struct PersonBatch<T> {
    rows: Vec<(T, u64, u64)>, // (row, user key, mix)
}
impl<T: Copy> PersonBatch<T> {
    fn new() -> Self {
        PersonBatch { rows: Vec::with_capacity(PBATCH) }
    }
    #[inline]
    fn push(&mut self, names: &Names, row: T, user: u64, emit: &mut impl FnMut(T, u64)) {
        let m = packed::fmix(user);
        names.prefetch(m);
        self.rows.push((row, user, m));
        if self.rows.len() == PBATCH {
            self.flush(names, emit);
        }
    }
    fn flush(&mut self, names: &Names, emit: &mut impl FnMut(T, u64)) {
        for &(row, user, m) in &self.rows {
            emit(row, names.person_mixed(m, user));
        }
        self.rows.clear();
    }
}

/// RW2: rows waiting for their person lookup; the table slots of a whole batch are
/// prefetched before any lookup runs, so the cache misses overlap.
const PBATCH: usize = 64; // v2: 32 -> 64 (A/B -0.9% / -2.3%)
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
    /// No linked IDs (every user is its own person).
    #[cfg(test)]
    pub fn empty() -> Names {
        Names { parts: (0..LP).map(|_| KeyMap::with_capacity(0)).collect() }
    }
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
            for_each_record_bq::<2>(chunk, |rec| {
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
            let mut m: IdMap = IdMap::with_capacity(n);
            let mut ids: Vec<S> = Vec::with_capacity(n);
            let mut slots: Vec<(u32, u32)> = Vec::with_capacity(n);
            for v in &scattered {
                for &(s, slot) in &v[p] {
                    let l = m.get_or_insert_with(s, || {
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
    // (RW19's parallel fill of `side` measured +3.8% at 60M on x86; reverted to sequential)
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
    // RW18: roots and person names in parallel. After union-find the parent array is fixed;
    // read-only finds are shallow (union by size). A person's name is the byte-wise smallest
    // member ID: for packed keys that is the smallest key (atomic fetch_min per root); the
    // rare interned IDs then get an exact byte-order pass.
    use std::sync::atomic::AtomicU64;
    let kid: Vec<u64> = ids.par_iter().map(|s| key_of(s)).collect();
    let parent_ro = &parent;
    let find_ro = |mut x: u32| -> u32 {
        while parent_ro[x as usize] != x {
            x = parent_ro[x as usize];
        }
        x
    };
    let root: Vec<u32> = (0..n as u32).into_par_iter().map(find_ro).collect();
    let min_key: Vec<AtomicU64> = (0..n).map(|_| AtomicU64::new(u64::MAX)).collect();
    (0..n).into_par_iter().for_each(|i| {
        let k = kid[i];
        if k & keys::INTERNED == 0 {
            min_key[root[i] as usize].fetch_min(k, Relaxed);
        }
    });
    let mut name_key: Vec<u64> = min_key.into_iter().map(AtomicU64::into_inner).collect();
    for i in 0..n {
        if kid[i] & keys::INTERNED != 0 {
            let r = root[i] as usize;
            let cur = name_key[r];
            if cur == u64::MAX || keys::cmp_bytes(kid[i], cur) == std::cmp::Ordering::Less {
                name_key[r] = kid[i];
            }
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
                let nk = name_key[root[i] as usize];
                if nk != kid[i] {
                    v.push((kid[i], nk)); // distinct IDs have distinct keys
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

/// Dedup over the copies of one partition: per key, keep the copy `better(new, kept)`
/// prefers (the first copy wins ties); `sink` receives each kept copy; returns how many.
/// RW13: a large partition is first split by a stable counting sort into sub-buckets of
/// about `SUB` copies (more hash bits), so each dedup table (~2*SUB slots) stays in L2.
/// Stability keeps scan order inside a sub-bucket, so ties still go to the first copy.
const SUB: usize = 8192;
fn dedup<T: Copy>(parts: &[Vec<T>], key: impl Fn(&T) -> u64, better: impl Fn(&T, &T) -> bool, mut sink: impl FnMut(&T)) -> usize {
    let n: usize = parts.iter().map(Vec::len).sum();
    // RW26: singleton filter. 2-bit saturating counters (0, 1, 2 = "two or more") over
    // ~8n hash slots. A row whose slot counted exactly 1 is the only copy of its key, so it
    // is kept as is; every other row (true duplicates and hash collisions) goes, in scan
    // order, through the exact table below, so tie-breaking is unchanged.
    if n > 2 * SUB {
        let bits = (n * 8).next_power_of_two().trailing_zeros();
        let slots = 1usize << bits;
        let mut cnt = vec![0u64; slots / 32]; // 32 two-bit counters per word
        let slot = |k: u64| -> usize { ((packed::fmix(k) >> 8) as usize) & (slots - 1) };
        for part in parts {
            for r in part {
                let i = slot(key(r));
                let (w, sh) = (i >> 5, (i & 31) * 2);
                let c = (cnt[w] >> sh) & 3;
                if c < 2 {
                    cnt[w] += 1 << sh;
                }
            }
        }
        let mut rest: Vec<T> = Vec::new();
        let mut kept = 0usize;
        for part in parts {
            for r in part {
                let i = slot(key(r));
                if (cnt[i >> 5] >> ((i & 31) * 2)) & 3 == 1 {
                    sink(r);
                    kept += 1;
                } else {
                    rest.push(*r);
                }
            }
        }
        drop(cnt);
        // the exact pass runs once over the leftovers (never the filter again)
        return kept + dedup_exact(std::slice::from_ref(&rest), key, better, sink);
    }
    dedup_exact(parts, key, better, sink)
}

/// The exact dedup (RW13): stable counting sort into L2-sized sub-buckets, one table each.
fn dedup_exact<T: Copy>(parts: &[Vec<T>], key: impl Fn(&T) -> u64, better: impl Fn(&T, &T) -> bool, mut sink: impl FnMut(&T)) -> usize {
    let n: usize = parts.iter().map(Vec::len).sum();
    {
    let sbits = if n <= 2 * SUB { 0 } else { (n / SUB).next_power_of_two().trailing_zeros() };
    // sub-bucket bits sit just below the partition bits of the same mix
    let sub = |k: u64| -> usize { if sbits == 0 { 0 } else { ((packed::fmix(k) << LOG_NB) >> (64 - sbits)) as usize } };
    let nsub = 1usize << sbits;
    let mut flat: Vec<T> = Vec::with_capacity(n);
    let mut starts = vec![0u32; nsub + 1];
    if sbits == 0 {
        for part in parts {
            flat.extend_from_slice(part);
        }
        starts[1] = n as u32;
    } else {
        for part in parts {
            for r in part {
                starts[sub(key(r)) + 1] += 1;
            }
        }
        for b in 1..=nsub {
            starts[b] += starts[b - 1];
        }
        let mut at = starts.clone();
        let spare = flat.spare_capacity_mut();
        for part in parts {
            for r in part {
                let b = sub(key(r));
                spare[at[b] as usize].write(*r);
                at[b] += 1;
            }
        }
        // SAFETY: the counting pass wrote each of the n slots exactly once
        unsafe { flat.set_len(n) };
    }
    let mut used = 0usize;
    let mut slots: Vec<(u64, u32)> = Vec::new();
    for b in 0..nsub {
        let rows = &flat[starts[b] as usize..starts[b + 1] as usize];
        let cap = (rows.len() * 2).next_power_of_two().max(16);
        let mask = cap - 1;
        slots.clear();
        slots.resize(cap, (keys::EMPTY, 0));
        for (ri, r) in rows.iter().enumerate() {
            let k = key(r);
            let mut i = packed::fmix(k) as usize & mask;
            loop {
                let s = &mut slots[i];
                if s.0 == keys::EMPTY {
                    *s = (k, ri as u32);
                    used += 1;
                    break;
                }
                if s.0 == k {
                    if better(r, &rows[s.1 as usize]) {
                        s.1 = ri as u32;
                    }
                    break;
                }
                i = (i + 1) & mask;
            }
        }
        for s in &slots {
            if s.0 != keys::EMPTY {
                sink(&rows[s.1 as usize]);
            }
        }
    }
    used
    }
}

/// RW31: click scan. Valid copies carry their *user* key in `person`; persons are resolved
/// in `clicks_dedup`, so this can run while links are still being resolved.
fn clicks_scan(src: &[&'static Source], campaigns: &IdMap, lo: i64, hi: i64, names: &Names, stats: &Stats) -> ByPerson<ClickV> {
    let mut st = Stage::start();
    let items = work_items::<4>(src, ["click_id", "user_id", "campaign_id", "ts"]);
    let scattered = clicks_scan_items(&items, campaigns, lo, hi, names, stats, fast_enabled());
    st.mark("clicks: scan+parse+partition");
    let by_person = transpose(scattered);
    st.mark("clicks: transpose");
    by_person
}

/// The per-chunk clicks scan: scan + validate each copy + partition the valid ones by person
/// (exact path, or v2 P1 fast path with exact fallback).
///
/// v2 P2: no click dedup. rules.md promises that copies of a click have identical values
/// (so the same user, person, ts, campaign and validity), and a copy cannot change any
/// output: credit goes to the latest qualifying click with the smallest click_id (a copy is
/// that same click) and assists count distinct campaigns. Invalid copies are dropped here.
fn clicks_scan_items(items: &[([usize; 4], S)], campaigns: &IdMap, lo: i64, hi: i64, names: &Names, stats: &Stats, fast_ok: bool) -> Vec<Vec<Vec<ClickV>>> {
    items
        .par_iter()
        .map(|(order, chunk)| {
            let mut sc = ClScan::new(chunk.len(), campaigns, lo, hi, names);
            if fast_ok && is_identity(order) {
                let len = chunk.len();
                let mut s = 0usize;
                while s < len {
                    if s + fast::SLACK <= len {
                        // SAFETY: s + SLACK <= len, so the decoder reads inside the chunk
                        if let Some((l, c)) = unsafe { fast::click(chunk.as_ptr().add(s), lo, hi) } {
                            if let Some(camp) = campaigns.get_packed(packed::fmix(c.camp), c.camp) {
                                sc.lines += 1;
                                sc.valid(c.cid, c.user, c.ts, camp);
                                s += l + 1;
                                continue;
                            }
                        }
                    }
                    let e = memchr::memchr(b'\n', &chunk[s..]).map_or(len, |i| s + i);
                    sc.exact(split::<4>(&chunk[s..e]), order);
                    s = e + 1;
                }
            } else {
                for_each_record_bq::<4>(chunk, |rec| sc.exact(rec, order));
            }
            sc.finish(stats)
        })
        .collect()
}

/// One chunk's clicks scan state (v2: monomorphic, like `EvScan`).
struct ClScan<'a> {
    parts: Vec<Vec<ClickV>>,
    // valid copies waiting for their person: (click, user key, user mix); the person slots of a
    // whole batch are prefetched before any lookup runs
    pend: [(ClickV, u64); PBATCH],
    np: usize,
    campaigns: &'a IdMap,
    names: &'a Names,
    lo: i64,
    hi: i64,
    lines: u64,
    bad: u64,
    noid: u64,
    why: [u64; 4],
}

impl<'a> ClScan<'a> {
    fn new(bytes: usize, campaigns: &'a IdMap, lo: i64, hi: i64, names: &'a Names) -> Self {
        const Z: ClickV = ClickV { person: 0, ts: 0, cid: 0, camp: 0 };
        ClScan {
            parts: (0..NB).map(|_| Vec::with_capacity(bytes / 24 / NB + 32)).collect(), // v2: clicks lines ~26 B
            pend: [(Z, 0); PBATCH],
            np: 0,
            campaigns,
            names,
            lo,
            hi,
            lines: 0,
            bad: 0,
            noid: 0,
            why: [0; 4],
        }
    }
    #[inline(always)]
    fn valid(&mut self, cid: u64, user: u64, ts: i64, camp: u32) {
        let um = packed::fmix(user);
        self.names.prefetch(um);
        // `person` holds the user key until the flush
        self.pend[self.np] = (ClickV { person: user, ts, cid, camp }, um);
        self.np += 1;
        if self.np == PBATCH {
            self.flush();
        }
    }
    #[inline]
    fn flush(&mut self) {
        for i in 0..self.np {
            let (mut c, um) = self.pend[i];
            c.person = self.names.person_mixed(um, c.person);
            self.parts[keys::part(c.person, LOG_NB)].push(c);
        }
        self.np = 0;
    }
    fn finish(mut self, stats: &Stats) -> Vec<Vec<ClickV>> {
        self.flush();
        stats.add(K::ClBadUser, self.why[1]);
        stats.add(K::ClBadTs, self.why[2]);
        stats.add(K::ClBadCampaign, self.why[3]);
        stats.add(K::ClSurvived, self.parts.iter().map(|v| v.len() as u64).sum());
        stats.add(K::ClLines, self.lines);
        stats.add(K::ClMalformed, self.bad);
        stats.add(K::ClNoId, self.noid);
        self.parts
    }
    /// The exact path for one line's fields (None = malformed).
    fn exact(&mut self, rec: Option<[S; 4]>, order: &[usize; 4]) {
        self.lines += 1;
        let Some(f) = rec else {
            self.bad += 1;
            return;
        };
        let f = reorder(f, order);
        if f[0].is_empty() {
            self.noid += 1;
            return;
        }
        let cid = key_of(f[0]);
        if f[1].is_empty() {
            self.why[1] += 1;
            return;
        }
        let ts = timefmt::parse_in(f[3], self.lo, self.hi);
        if ts == BAD {
            self.why[2] += 1;
            return;
        }
        match self.campaigns.get(f[2]) {
            None => self.why[3] += 1,
            Some(c) => self.valid(cid, key_of(f[1]), ts, c),
        }
    }
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

/// v2: a valid, deduplicated event as the sessions phase reads it. Two layouts share the
/// code: `EvV` (any input) and the 16-byte `EvC`, used when every value fits it (event ts
/// offsets from window_start below 2^32 ms and at most 256 categories; checked in `main`).
pub trait EvLike: Copy + Send + Sync + 'static {
    /// `base` is window_start (the compact layout stores ts - base).
    fn make(person: u64, ts: i64, cat: u32, price: u32, base: i64) -> Self;
    fn person(&self) -> u64;
    fn ts(&self, base: i64) -> i64;
    fn cat(&self) -> u32;
    fn price(&self) -> u32;
    /// Sort key: person, then ts (unsigned order = (person, ts) order).
    fn key(&self) -> u128;
}

impl EvLike for EvV {
    #[inline(always)]
    fn make(person: u64, ts: i64, cat: u32, price: u32, _: i64) -> Self {
        EvV { person, ts, cat, price }
    }
    #[inline(always)]
    fn person(&self) -> u64 {
        self.person
    }
    #[inline(always)]
    fn ts(&self, _: i64) -> i64 {
        self.ts
    }
    #[inline(always)]
    fn cat(&self) -> u32 {
        self.cat
    }
    #[inline(always)]
    fn price(&self) -> u32 {
        self.price
    }
    #[inline(always)]
    fn key(&self) -> u128 {
        ((self.person as u128) << 64) | ((self.ts as u64) ^ (1 << 63)) as u128
    }
}

/// v2: compact deduplicated event (16 bytes): ts as an offset from window_start, price (< 2^24)
/// and category index (< 256) in one word.
#[derive(Clone, Copy)]
pub struct EvC {
    pub person: u64,
    pub ts: u32,
    pub pc: u32,
}

impl EvLike for EvC {
    #[inline(always)]
    fn make(person: u64, ts: i64, cat: u32, price: u32, base: i64) -> Self {
        debug_assert!((0..=u32::MAX as i64).contains(&(ts - base)) && cat < 256 && price < 1 << 24);
        EvC { person, ts: (ts - base) as u32, pc: price << 8 | cat }
    }
    #[inline(always)]
    fn person(&self) -> u64 {
        self.person
    }
    #[inline(always)]
    fn ts(&self, base: i64) -> i64 {
        base + self.ts as i64
    }
    #[inline(always)]
    fn cat(&self) -> u32 {
        self.pc & 0xff
    }
    #[inline(always)]
    fn price(&self) -> u32 {
        self.pc >> 8
    }
    #[inline(always)]
    fn key(&self) -> u128 {
        ((self.person as u128) << 64) | self.ts as u128
    }
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
/// RW31: event scan. Valid copies carry their *user* key in `person`; persons are resolved
/// in `events_dedup`, so this can run while links are still being resolved.
fn events_scan(src: &[&'static Source], products: &IdMap, unknown_cat: u32, lo: i64, hi: i64, names: Option<&Names>, stats: &Stats) -> ByPerson<EvRec> {
    const NAMES: [&str; 7] = ["event_id", "user_id", "ts", "ingest_ts", "event_type", "product_id", "price_cents"];
    let mut st = Stage::start();
    let items = work_items::<7>(src, NAMES);
    let scattered = events_scan_items(&items, products, unknown_cat, lo, hi, names, stats, fast_enabled());
    st.mark("events: scan+parse+partition");
    let by_eid = transpose(scattered);
    st.mark("events: transpose");
    by_eid
}

/// The per-chunk events scan (exact path, or v2 P1 fast path with exact fallback).
#[allow(clippy::too_many_arguments)]
fn events_scan_items(items: &[([usize; 7], S)], products: &IdMap, unknown_cat: u32, lo: i64, hi: i64, names: Option<&Names>, stats: &Stats, fast_ok: bool) -> Vec<Vec<Vec<EvRec>>> {
    items
        .par_iter()
        .map(|(order, chunk)| {
            let mut sc = EvScan::new(chunk.len(), products, unknown_cat, lo, hi, names);
            if fast_ok && is_identity(order) {
                // v2 P1: shape-specialized fast path; every line it declines takes `exact`.
                // v2: line ends come from one bulk (vectorized) newline pass first, so each
                // line's decode starts from a known offset instead of waiting on the previous
                // line's newline search.
                let len = chunk.len();
                let base = chunk.as_ptr();
                let ends: Vec<u32> = memchr::memchr_iter(b'\n', chunk).map(|i| i as u32).collect();
                let mut s = 0usize;
                for &e in &ends {
                    let e = e as usize;
                    if s + fast::SLACK <= len {
                        // SAFETY: s + SLACK <= len, so the decoder reads inside the chunk
                        if let Some((_, ev)) = unsafe { fast::event(base.add(s), lo, hi, products.dense_prefix()) } {
                            // (the decoder's line end is the first '\n' after s, i.e. e)
                            sc.lines += 1;
                            let r = EvRec { eid: ev.eid, rank: rank_of(Some(ev.ingest)), person: ev.user, ts: ev.ts, price: ev.price, cat: 0 };
                            if ev.pnum != 0 {
                                sc.valid(EvRec { cat: products.get_dense(ev.pnum).unwrap_or(unknown_cat), ..r }, 0, 0);
                            } else {
                                let m = packed::fmix(ev.product);
                                products.prefetch_packed(m);
                                sc.valid(r, ev.product, m);
                            }
                            s = e + 1;
                            continue;
                        }
                    }
                    sc.exact(split::<7>(&chunk[s..e]), order);
                    s = e + 1;
                }
                if s < len {
                    sc.exact(split::<7>(&chunk[s..len]), order); // final line without '\n'
                }
            } else {
                for_each_record_bq::<7>(chunk, |rec| sc.exact(rec, order));
            }
            sc.finish(stats)
        })
        .collect()
}

/// One chunk's events scan state: partitions by event_id, the product-lookup batch, counters.
struct EvScan<'a> {
    parts: Vec<Vec<EvRec>>,
    // RW15/RW32: (row with its user key in `person`, product key or 0, product mix); the
    // product slots of a whole batch are prefetched before any lookup runs
    pend: [(EvRec, u64, u64, u64); PBATCH],
    // None: persons are resolved later (events dedup); `person` keeps the user key
    names: Option<&'a Names>,
    np: usize,
    products: &'a IdMap,
    unknown_cat: u32,
    lo: i64,
    hi: i64,
    lines: u64,
    bad: u64,
    noid: u64,
}

impl<'a> EvScan<'a> {
    fn new(bytes: usize, products: &'a IdMap, unknown_cat: u32, lo: i64, hi: i64, names: Option<&'a Names>) -> Self {
        const Z: EvRec = EvRec { eid: 0, rank: 0, person: 0, ts: 0, price: 0, cat: 0 };
        EvScan {
            parts: (0..NB).map(|_| Vec::with_capacity(bytes / 60 / NB + 32)).collect(), // v2: event lines ~67 B
            pend: [(Z, 0, 0, 0); PBATCH],
            names,
            np: 0,
            products,
            unknown_cat,
            lo,
            hi,
            lines: 0,
            bad: 0,
            noid: 0,
        }
    }
    #[inline(always)]
    fn put(&mut self, r: EvRec) {
        self.parts[keys::part(r.eid, LOG_NB)].push(r);
    }
    #[inline(always)]
    fn valid(&mut self, r: EvRec, pk: u64, pm: u64) {
        // v2: links are resolved before the scan, so the person lookup joins this batch
        // (its slot prefetched now, read at flush); `r.person` holds the user key until then
        // v2: um = 0 (the mix of no valid key) marks "not linked" (filter bit clear)
        // um = 0 (the mix of no valid key) marks "no lookup" (persons resolved later)
        let mut um = 0;
        if let Some(names) = self.names {
            um = packed::fmix(r.person);
            names.prefetch(um);
        }
        self.pend[self.np] = (r, pk, pm, um);
        self.np += 1;
        if self.np == PBATCH {
            self.flush();
        }
    }
    #[inline]
    fn flush(&mut self) {
        for i in 0..self.np {
            let (mut r, pk, pm, um) = self.pend[i];
            if um != 0 {
                r.person = self.names.unwrap().person_mixed(um, r.person);
            }
            if pk != 0 {
                r.cat = self.products.get_packed(pm, pk).unwrap_or(self.unknown_cat);
            }
            self.put(r);
        }
        self.np = 0;
    }
    fn finish(mut self, stats: &Stats) -> Vec<Vec<EvRec>> {
        self.flush();
        stats.add(K::EvLines, self.lines);
        stats.add(K::EvMalformed, self.bad);
        stats.add(K::EvNoId, self.noid);
        self.parts
    }
    /// The exact path for one line's fields (None = malformed).
    fn exact(&mut self, rec: Option<[S; 7]>, order: &[usize; 7]) {
        let (lo, hi) = (self.lo, self.hi);
        self.lines += 1;
        let Some(f) = rec else {
            self.bad += 1;
            return;
        };
        let f = reorder(f, order);
        if f[0].is_empty() {
            self.noid += 1;
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
                        let (pk, pm) = match pack9(f[5]) {
                            _ if packed::canon(f[5], self.products.dense_prefix()).is_some() => {
                                r.cat = self.products.get(f[5]).unwrap_or(self.unknown_cat);
                                (0, 0)
                            }
                            Some(k) => {
                                let m = packed::fmix(k);
                                self.products.prefetch_packed(m);
                                (k, m)
                            }
                            None => {
                                r.cat = self.products.get(f[5]).unwrap_or(self.unknown_cat);
                                (0, 0)
                            }
                        };
                        r.person = key_of(f[1]);
                        self.valid(r, pk, pm);
                        return;
                    }
                    _ => r.price = 5,
                }
            }
        }
        self.put(r); // invalid copy: person stays EMPTY
    }
}

/// v2: the fast paths are on unless SS_NOFAST is set (A/B and debugging only).
fn fast_enabled() -> bool {
    std::env::var_os("SS_NOFAST").is_none()
}

/// RW31: event dedup + person resolution (batched) + repartition by person.
fn events_dedup<V: EvLike>(by_eid: ByPerson<EvRec>, unknown_cat: u32, names: Option<&Names>, base: i64, stats: &Stats) -> ByPerson<V> {
    let mut st = Stage::start();
    let out: Vec<Vec<Vec<V>>> = by_eid
        .into_par_iter()
        .map(|parts| {
            let n: usize = parts.iter().map(Vec::len).sum();
            // rules.md, Events step 2: keep the copy with the smallest ingest_ts; A2: on a
            // tie the first copy stays (equal-ingest copies are promised identical)
            let mut dst: Vec<Vec<V>> = (0..NB).map(|_| Vec::with_capacity(n / NB + n / NB / 8 + 32)).collect(); // v2: room for the spread (no regrowth)
            let mut c = [0u64; 8];
            let mut pb: PersonBatch<EvRec> = PersonBatch::new();
            let mut emit = |r: EvRec, person: u64| {
                dst[keys::part(person, LOG_NB)].push(V::make(person, r.ts, r.cat, r.price, base));
            };
            let kept = dedup(&parts, |r: &EvRec| r.eid, |a, b| a.rank < b.rank, |r| {
                if r.person == keys::EMPTY {
                    c[r.price as usize] += 1;
                    return;
                }
                if r.cat == unknown_cat {
                    c[6] += 1;
                }
                match names {
                    Some(n) => pb.push(n, *r, r.person, &mut emit), // the scan kept the user key
                    None => emit(*r, r.person),                      // the scan resolved it
                }
            });
            if let Some(n) = names {
                pb.flush(n, &mut emit);
            }
            drop(emit);
            stats.add(K::EvDupCopies, (n - kept) as u64);
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

/// RW6: counting sort of the rows of all `parts` into `nbk` buckets, then sort each bucket
/// with `cmp`. The result is ordered by (bucket, cmp).
fn bucket_sort<T: Copy>(parts: &[Vec<T>], n: usize, nbk: usize, bucket: impl Fn(&T) -> usize, sort: impl Fn(&mut [T])) -> (Vec<T>, Vec<u32>) {
    // RW27: each row's bucket is computed once (kept in `bk`) and reused for placement
    let mut bk: Vec<u32> = Vec::with_capacity(n);
    let mut cnt = vec![0u32; nbk + 1];
    for part in parts {
        for r in part {
            let k = bucket(r);
            bk.push(k as u32);
            cnt[k + 1] += 1;
        }
    }
    for i in 1..=nbk {
        cnt[i] += cnt[i - 1];
    }
    let starts = cnt.clone();
    let mut out: Vec<T> = Vec::with_capacity(n);
    let spare = out.spare_capacity_mut();
    let mut idx = 0usize;
    for part in parts {
        for r in part {
            let k = bk[idx] as usize;
            idx += 1;
            spare[cnt[k] as usize].write(*r);
            cnt[k] += 1;
        }
    }
    // SAFETY: the counting pass wrote every one of the n slots exactly once
    unsafe { out.set_len(n) };
    for k in 0..nbk {
        sort(&mut out[starts[k] as usize..starts[k + 1] as usize]);
    }
    (out, starts)
}

#[allow(clippy::too_many_arguments)]
fn sessionize<V: EvLike>(
    b: usize,
    evs: &ByPerson<V>,
    cls: &ByPerson<ClickV>,
    ncamp: usize,
    day0: i32,
    ndays: usize,
    camp: &mut [[u64; 3]],
    assist: &mut [u32],
    stats: &Stats,
    tb: i64,
) -> Vec<output::UserRow> {
    // RW6: events and clicks are grouped by bucket (bits of fmix(person) below the
    // partition bits) with one counting-sort pass, then each small bucket is sorted by
    // (person, ts). The walk only needs every person's rows together and time-ordered,
    // and buckets come in the same order for events and clicks, so the per-person merge
    // below still lines up (within a bucket, persons are in key order on both sides).
    let n: usize = evs[b].iter().map(Vec::len).sum();
    let nc: usize = cls[b].iter().map(Vec::len).sum();
    let nbk: usize = (n / 64).next_power_of_two().clamp(1, 1 << 14);
    let bshift = 64 - LOG_NB - nbk.trailing_zeros();
    let bucket = |person: u64| -> usize { ((packed::fmix(person) >> bshift) as usize) & (nbk - 1) };
    // events sort on one u128 key: person, then ts (sign-flipped so unsigned order = signed)
    let (ev, es) = bucket_sort(&evs[b], n, nbk, |e: &V| bucket(e.person()), |v: &mut [V]| v.sort_unstable_by_key(|e| e.key()));
    // clicks: (person, ts asc, click_id desc in byte order), so the last click of a
    // window is the latest one with the byte-wise smallest click_id
    let (cl, cs) = bucket_sort(&cls[b], nc, nbk, |c: &ClickV| bucket(c.person), |v: &mut [ClickV]| {
        v.sort_unstable_by(|x, y| (x.person, x.ts).cmp(&(y.person, y.ts)).then_with(|| keys::cmp_bytes(y.cid, x.cid)))
    });

    let mut rows: Vec<output::UserRow> = Vec::with_capacity(n / 8 + 16);
    // sliding-window distinct-campaign set: count per campaign + list of active ones
    let mut cnt = vec![0u32; ncamp];
    let mut pos = vec![u32::MAX; ncamp];
    let mut active: Vec<u32> = Vec::new();
    let (mut sessions_total, mut credited, mut assists, mut persons) = (0u64, 0u64, 0u64, 0u64);

    let mut cat_rev: Vec<(u32, u64)> = Vec::new();
    // RW27: walk bucket by bucket; inside a bucket both sides are sorted by person, so the
    // per-person merge needs no hashing
    for bkt in 0..nbk {
    let (e1, c1) = (es[bkt + 1] as usize, cs[bkt + 1] as usize);
    let mut i = es[bkt] as usize; // event cursor
    let mut j = cs[bkt] as usize; // click cursor (first click of current person)
    while i < e1 {
        let p = ev[i].person();
        persons += 1;
        let mut pe = i + 1;
        while pe < e1 && ev[pe].person() == p {
            pe += 1;
        }
        while j < c1 && cl[j].person < p {
            j += 1;
        }
        let mut pc = j;
        while pc < c1 && cl[pc].person == p {
            pc += 1;
        }
        let (mut lo, mut hi) = (j, j); // window [lo, hi) of this person's clicks

        let mut day_cur: i32 = i32::MIN;
        let (mut d_sessions, mut d_events, mut d_rev) = (0u64, 0u64, 0u64);
        cat_rev.clear();

        let mut k = i;
        while k < pe {
            // one session: [k, e); equal timestamps never split (gap 0)
            let start = ev[k].ts(tb);
            let mut e = k + 1;
            while e < pe && ev[e].ts(tb) - ev[e - 1].ts(tb) <= GAP {
                e += 1;
            }
            let day = start.div_euclid(DAY) as i32;
            let (mut rev, mut has_p) = (0u64, false);
            for x in &ev[k..e] {
                if x.price() > 0 {
                    has_p = true;
                    rev += x.price() as u64;
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
                assist[c as usize * ndays + doff] += 1;
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
                    if x.price() > 0 {
                        match cat_rev.iter_mut().find(|c| c.0 == x.cat()) {
                            Some(c) => c.1 += x.price() as u64,
                            None => cat_rev.push((x.cat(), x.price() as u64)),
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

#[cfg(test)]
mod dedup_tests {
    /// RW26: dedup (singleton filter + exact pass) equals a naive first-min reference on
    /// mixes of singletons and heavy duplicates, across several source parts.
    #[test]
    fn dedup_matches_naive() {
        let mut x = 17u64;
        let mut rnd = || {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            x >> 33
        };
        for case in 0..6 {
            // rows: (key, rank, id); smaller rank wins, ties keep the first seen
            let nparts = 1 + case % 3;
            let mut parts: Vec<Vec<(u64, u64, u32)>> = vec![Vec::new(); nparts];
            let total = [1_000usize, 40_000, 120_000, 60_000, 90_000, 30_000][case];
            for id in 0..total as u32 {
                let k = match case {
                    3 => 1 + rnd() % 4,                   // 60k rows on 4 keys
                    4 => if id % 2 == 0 { 1 + rnd() % 7 } else { 1000 + id as u64 },
                    _ => 1 + rnd() % (total as u64 / 3 + 1),
                };
                parts[(rnd() as usize) % nparts].push((k, rnd() % 5, id));
            }
            let mut got: Vec<(u64, u64, u32)> = Vec::new();
            let kept = super::dedup(&parts, |r| r.0, |a, b| a.1 < b.1, |r| got.push(*r));
            let mut want: std::collections::HashMap<u64, (u64, u64, u32)> = std::collections::HashMap::new();
            for p in &parts {
                for r in p {
                    want.entry(r.0).and_modify(|c| if r.1 < c.1 { *c = *r }).or_insert(*r);
                }
            }
            let mut w: Vec<_> = want.into_values().collect();
            got.sort();
            w.sort();
            assert_eq!(kept, w.len(), "case {case}");
            assert_eq!(got, w, "case {case}");
        }
    }
}

#[cfg(test)]
mod fast_scan_tests {
    use super::*;

    fn flat(v: Vec<Vec<Vec<EvRec>>>) -> Vec<Vec<(u64, u64, u64, i64, u32, u32)>> {
        v.into_iter().map(|c| c.into_iter().flatten().map(|r| (r.eid, r.rank, r.person, r.ts, r.price, r.cat)).collect()).collect()
    }

    /// v2 P1: the events scan with the fast path gives exactly the records (same order, same
    /// partitions) and stats as the exact path, on random dirty chunks.
    #[test]
    fn events_fast_equals_exact() {
        let lo = timefmt::parse(b"2026-10-01T00:00:00Z").unwrap();
        let hi = lo + 14 * DAY;
        let mut products = IdMap::with_dense(100, b'p');
        for i in 0..50_000u32 {
            let s: S = Box::leak(format!("p{i}").into_bytes().into_boxed_slice());
            products.insert_first(s, i % 21);
        }
        let mut x = 77u64;
        let mut lcg = || {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            x >> 33
        };
        let mut y = 5u64;
        for case in 0..40 {
            let mut buf: Vec<u8> = Vec::new();
            let nl = 1 + lcg() % 3000;
            for _ in 0..nl {
                match lcg() % 100 {
                    0 => buf.extend(std::iter::repeat_n(b'a', 100 + (lcg() % 100) as usize)),
                    1 => {}
                    2 => buf.extend_from_slice(b"e1,u1,2026-10-02T00:00:00Z,2026-10-02T00:00:00Z,view,p1,"),
                    _ => buf.extend(fast::tests_support::random_event_line(&mut y, lo)),
                }
                buf.push(b'\n');
            }
            if case % 3 == 0 {
                buf.pop(); // no final newline
            }
            let chunk: S = Box::leak(buf.into_boxed_slice());
            let items = vec![([0, 1, 2, 3, 4, 5, 6], chunk)];
            let (s1, s2) = (Stats::default(), Stats::default());
            let a = flat(events_scan_items(&items, &products, 20, lo, hi, Some(&Names::empty()), &s1, true));
            let b = flat(events_scan_items(&items, &products, 20, lo, hi, Some(&Names::empty()), &s2, false));
            assert_eq!(a, b, "case {case}");
            assert_eq!(s1.json(), s2.json(), "case {case}");
        }
    }
}

#[cfg(test)]
mod fast_click_tests {
    use super::*;

    /// v2 P1: the clicks scan with the fast path equals the exact path (records, order, stats).
    #[test]
    fn clicks_fast_equals_exact() {
        let lo = timefmt::parse(b"2026-10-01T00:00:00Z").unwrap();
        let hi = lo + 14 * DAY;
        let mut camps = IdMap::with_capacity(100);
        for i in 1..=2000u32 {
            let s: S = Box::leak(format!("c_{i}").into_bytes().into_boxed_slice());
            camps.insert_first(s, i - 1);
        }
        let mut x = 99u64;
        let mut lcg = || {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            x >> 33
        };
        let pick = |r: u64, v: &[&str]| v[(r as usize) % v.len()].to_string();
        for case in 0..40 {
            let mut buf = String::new();
            for _ in 0..(1 + lcg() % 3000) {
                let cid = if lcg() % 40 == 0 { pick(lcg(), &["", "k1234567890", "K1", "é"]) } else { format!("k{:x}", lcg() % 100_000_000) };
                let uid = if lcg() % 40 == 0 { pick(lcg(), &["", "U1", " u1", "u123456789x"]) } else { format!("u{:x}", lcg() % 1_000_000) };
                let camp = if lcg() % 30 == 0 { pick(lcg(), &["", "C_1", "c_2001", "c_01", "c_9999"]) } else { format!("c_{}", 1 + lcg() % 2000) };
                let ms = lo - 9 * DAY + (lcg() as i64 * 7919) % (25 * DAY);
                let ts = match lcg() % 6 {
                    0 => format!("{ms}"),
                    1 => pick(lcg(), &["", "2026-10-03T23:58:10z", "1.5", "2026-09-30T00:00:00Z\r"]),
                    _ => {
                        let d = ms.div_euclid(DAY);
                        let base = timefmt::days_from_civil(2026, 9, 1);
                        // a valid F1 near the window (day arithmetic via the parser's inverse is overkill)
                        let dd = (d - base).clamp(0, 60);
                        let (m, day) = if dd < 30 { (9, dd + 1) } else if dd < 61 { (10, dd - 29) } else { (11, 1) };
                        format!("2026-{m:02}-{day:02}T{:02}:{:02}:{:02}Z", lcg() % 24, lcg() % 60, lcg() % 60)
                    }
                };
                let fields = [cid, uid, camp, ts];
                for (i, f) in fields.iter().enumerate() {
                    if i > 0 {
                        buf.push(',');
                    }
                    match lcg() % 10 {
                        0 | 1 => buf.push_str(&format!("\"{f}\"")),
                        2 if lcg() % 10 == 0 => buf.push_str(&format!("{f}\"")),
                        _ => buf.push_str(f),
                    }
                }
                if lcg() % 200 == 0 {
                    buf.push_str(",x");
                }
                buf.push('\n');
            }
            if case % 4 == 1 {
                buf.pop();
            }
            let chunk: S = Box::leak(buf.into_bytes().into_boxed_slice());
            let items = vec![([0, 1, 2, 3], chunk)];
            let (s1, s2) = (Stats::default(), Stats::default());
            let f = |v: Vec<Vec<Vec<ClickV>>>| -> Vec<Vec<(u64, u64, i64, u32)>> {
                v.into_iter().map(|c| c.into_iter().flatten().map(|r| (r.cid, r.person, r.ts, r.camp)).collect()).collect()
            };
            let a = f(clicks_scan_items(&items, &camps, lo - 7 * DAY, hi, &Names::empty(), &s1, true));
            let b = f(clicks_scan_items(&items, &camps, lo - 7 * DAY, hi, &Names::empty(), &s2, false));
            assert_eq!(a, b, "case {case}");
            assert_eq!(s1.json(), s2.json(), "case {case}");
        }
    }
}

#[cfg(test)]
mod compact_tests {
    use super::*;

    /// v2: sessionize over the 16-byte layout equals the 24-byte layout (rows, campaign
    /// accumulators, stats), with window-edge timestamps, 30-minute edges, max prices and
    /// category 255.
    #[test]
    fn compact_layout_equals_wide() {
        let ws = timefmt::parse(b"2028-02-20T00:00:00Z").unwrap();
        let (day0, ndays, ncamp) = ((ws / DAY) as i32, 14usize, 40usize);
        let mut x = 5u64;
        let mut r = || {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            x >> 33
        };
        for case in 0..10 {
            let mut wide: Vec<EvV> = Vec::new();
            for _ in 0..(r() % 30_000) {
                let p = 1 + r() % (1 + r() % 2000);
                let t = match r() % 10 {
                    0 => ws,
                    1 => ws + 14 * DAY - 1,
                    _ => ws + (r() as i64 * 977) % (14 * DAY),
                };
                let t = if r() % 6 == 0 { t - t % GAP } else { t };
                let price = match r() % 40 { 0 => 10_000_000, 1..=3 => 1 + (r() % 50_000) as u32, _ => 0 };
                wide.push(EvV { person: p * 104729 + case, ts: t.max(ws), cat: if r() % 50 == 0 { 255 } else { (r() % 21) as u32 }, price });
            }
            let mut cl: Vec<ClickV> = Vec::new();
            for _ in 0..(r() % 8_000) {
                let p = 1 + r() % 2100;
                cl.push(ClickV { person: p * 104729 + case, ts: ws - WEEK + (r() as i64 * 7907) % (21 * DAY), cid: 1 + r() % 300, camp: (r() % ncamp as u64) as u32 });
            }
            let comp: Vec<EvC> = wide.iter().map(|e| EvC::make(e.person, e.ts, e.cat, e.price, ws)).collect();
            fn part<T: Clone>(v: Vec<T>) -> ByPerson<T> {
                let mut a: ByPerson<T> = (0..NB).map(|_| Vec::new()).collect();
                let h = v.len() / 3;
                a[0] = vec![v[..h].to_vec(), v[h..].to_vec()];
                a
            }
            let cls = part(cl);
            fn run<V: EvLike>(evs: &ByPerson<V>, cls: &ByPerson<ClickV>, ncamp: usize, day0: i32, ndays: usize, ws: i64) -> (Vec<(u64, i32, u64, u64, u64, u32)>, Vec<[u64; 3]>, Vec<u32>, String) {
                let (mut camp, mut assist) = (vec![[0u64; 3]; (ncamp + 1) * ndays], vec![0u32; (ncamp + 1) * ndays]);
                let st = Stats::default();
                let rows = sessionize(0, evs, cls, ncamp, day0, ndays, &mut camp, &mut assist, &st, ws);
                let mut rows: Vec<_> = rows.iter().map(|u| (u.user, u.day, u.sessions, u.events, u.revenue, u.top)).collect();
                rows.sort();
                (rows, camp, assist, st.json())
            }
            assert!(run(&part(wide), &cls, ncamp, day0, ndays, ws) == run(&part(comp), &cls, ncamp, day0, ndays, ws), "case {case}");
        }
        assert_eq!(std::mem::size_of::<EvC>(), 16);
    }
}
