//! Output relations, written as Parquet with exactly the required columns.
//!
//! Each file is written under a dot-prefixed temporary name (ignored by the checker)
//! and renamed into place only once complete, so a reader never sees a torn file.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow_array::builder::{Date32Builder, Int64Builder, StringBuilder};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::{DataType, Field, Schema};
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::{EnabledStatistics, WriterProperties};
use rayon::prelude::*;

use crate::{Map, NO_CAT, ORGANIC};

type S = &'static [u8];

/// One row of user_daily. Key: (user_id, day). RW1: `user` is the person's u64 key;
/// the name is decoded from it only when writing.
pub struct UserRow {
    pub user: u64,
    pub day: i32,
    pub sessions: u64,
    pub events: u64,
    pub revenue: u64,
    pub top: u32, // category index (byte order), NO_CAT for null
}

impl UserRow {
    /// `cat_rev` holds summed purchase price per category index; the top category is
    /// the highest sum, ties to the smallest index (= byte-wise smallest name).
    pub fn new(user: u64, day: i32, sessions: u64, events: u64, revenue: u64, cat_rev: &[(u32, u64)]) -> UserRow {
        let mut top = NO_CAT;
        let mut best = 0u64;
        for &(c, r) in cat_rev {
            if r > best || (r == best && r > 0 && c < top) {
                best = r;
                top = c;
            }
        }
        UserRow { user, day, sessions, events, revenue, top }
    }
}

fn utf8(b: &[u8]) -> std::borrow::Cow<'_, str> {
    String::from_utf8_lossy(b)
}

fn props() -> WriterProperties {
    WriterProperties::builder()
        .set_compression(Compression::UNCOMPRESSED)
        .set_statistics_enabled(EnabledStatistics::None)
        .set_max_row_group_row_count(Some(1 << 20))
        // v2: user_id is nearly unique; a dictionary attempt is wasted work (it falls back
        // to PLAIN after the first 1 MB dictionary page anyway)
        .set_column_dictionary_enabled(parquet::schema::types::ColumnPath::from("user_id"), false)
        .build()
}

fn write_batch(dir: &Path, name: &str, batch: RecordBatch) {
    let tmp = dir.join(format!(".{name}.tmp"));
    let dst = dir.join(name);
    let file = File::create(&tmp).unwrap_or_else(|e| panic!("create {}: {e}", tmp.display()));
    let mut w = ArrowWriter::try_new(file, batch.schema(), Some(props())).expect("parquet writer");
    w.write(&batch).expect("parquet write");
    w.close().expect("parquet close");
    std::fs::rename(&tmp, &dst).expect("rename output");
}

pub fn user_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("user_id", DataType::Utf8, false),
        Field::new("day", DataType::Date32, false),
        Field::new("sessions", DataType::Int64, false),
        Field::new("events", DataType::Int64, false),
        Field::new("revenue_cents", DataType::Int64, false),
        Field::new("top_category", DataType::Utf8, true),
    ]))
}

pub fn campaign_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("campaign_id", DataType::Utf8, false),
        Field::new("channel", DataType::Utf8, false),
        Field::new("day", DataType::Date32, false),
        Field::new("sessions", DataType::Int64, false),
        Field::new("purchasing_sessions", DataType::Int64, false),
        Field::new("revenue_cents", DataType::Int64, false),
        Field::new("assisted_sessions", DataType::Int64, false),
    ]))
}

fn user_batch(rows: &[&UserRow], cats: &[S]) -> RecordBatch {
    let n = rows.len();
    let mut u = StringBuilder::with_capacity(n, n * 10);
    let mut d = Date32Builder::with_capacity(n);
    let mut s = Int64Builder::with_capacity(n);
    let mut e = Int64Builder::with_capacity(n);
    let mut r = Int64Builder::with_capacity(n);
    let mut t = StringBuilder::with_capacity(n, n * 8);
    for row in rows {
        let mut buf = [0u8; 9];
        u.append_value(utf8(crate::keys::bytes_of(row.user, &mut buf)));
        d.append_value(row.day);
        s.append_value(row.sessions as i64);
        e.append_value(row.events as i64);
        r.append_value(row.revenue as i64);
        if row.top == NO_CAT {
            t.append_null();
        } else {
            t.append_value(utf8(cats[row.top as usize]));
        }
    }
    let cols: Vec<ArrayRef> = vec![
        Arc::new(u.finish()),
        Arc::new(d.finish()),
        Arc::new(s.finish()),
        Arc::new(e.finish()),
        Arc::new(r.finish()),
        Arc::new(t.finish()),
    ];
    RecordBatch::try_new(user_schema(), cols).expect("user batch")
}

pub fn write_all(
    out: &Path,
    user_parts: Vec<Vec<UserRow>>,
    camp: Map<(u32, i32), [u64; 4]>,
    cats: &[S],
    camp_ids: &[S],
    channels: &[S],
    ncpu: usize,
) {
    let ud = out.join("user_daily");
    let cd = out.join("campaign_daily");
    std::fs::create_dir_all(&ud).expect("mkdir user_daily");
    std::fs::create_dir_all(&cd).expect("mkdir campaign_daily");

    // user_daily: split into ~ncpu files of similar size, written in parallel
    let total: usize = user_parts.iter().map(Vec::len).sum();
    let nfiles = ncpu.clamp(1, 16).min(total.max(1));
    let per = total.div_ceil(nfiles).max(1);
    let all: Vec<&UserRow> = user_parts.iter().flatten().collect();
    let groups: Vec<&[&UserRow]> = if all.is_empty() { vec![&all[..]] } else { all.chunks(per).collect() };
    groups.par_iter().enumerate().for_each(|(i, g)| {
        write_batch(&ud, &format!("part-{i:05}.parquet"), user_batch(g, cats));
    });

    // campaign_daily: small, one file
    let n = camp.len();
    let mut ci = StringBuilder::with_capacity(n, n * 8);
    let mut ch = StringBuilder::with_capacity(n, n * 8);
    let mut d = Date32Builder::with_capacity(n);
    let mut cols4: [Int64Builder; 4] = std::array::from_fn(|_| Int64Builder::with_capacity(n));
    for ((c, day), v) in camp {
        if c == ORGANIC {
            ci.append_value("organic");
            ch.append_value("organic");
        } else {
            ci.append_value(utf8(camp_ids[c as usize]));
            ch.append_value(utf8(channels[c as usize]));
        }
        d.append_value(day);
        for k in 0..4 {
            cols4[k].append_value(v[k] as i64);
        }
    }
    let [mut a, mut b, mut c, mut e] = cols4;
    let cols: Vec<ArrayRef> = vec![
        Arc::new(ci.finish()),
        Arc::new(ch.finish()),
        Arc::new(d.finish()),
        Arc::new(a.finish()),
        Arc::new(b.finish()),
        Arc::new(c.finish()),
        Arc::new(e.finish()),
    ];
    write_batch(&cd, "part-00000.parquet", RecordBatch::try_new(campaign_schema(), cols).expect("campaign batch"));
}
