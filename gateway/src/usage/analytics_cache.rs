//! Persisted aggregates advance by row ID; live counters never enter this cache.
use super::{aggregate_json, empty_totals, MAX_OPTIONS};
use crate::usage::{
    requests::{Query, ATTEMPT_ROWS, FILTERS},
    StoreData,
};
use anyhow::{ensure, Result};
use rusqlite::{types::Value as SqlValue, Connection};
use serde_json::{json, Value};
use std::collections::{btree_map::Entry, BTreeMap, BTreeSet};

// The branches are disjoint: appended rows, then previously excluded time ranges.
// NOT INDEXED keeps the append branch on the integer primary-key cursor.
const DELTA: &str = r#"usage AS (
    SELECT * FROM main.usage NOT INDEXED WHERE attempt>?9 AND started_at>=?1 AND started_at<?2
    UNION ALL SELECT * FROM main.usage WHERE attempt<=?9 AND started_at>=?1 AND started_at<?10
    UNION ALL SELECT * FROM main.usage WHERE attempt<=?9 AND started_at>=?11 AND started_at<?2
)"#;

#[derive(Clone)]
pub(super) struct Group {
    pub(super) attempt: i64,
    pub(super) totals: Value,
}

#[derive(Clone)]
pub(super) struct Options {
    sources: BTreeMap<String, (i64, String)>,
    models: BTreeSet<String>,
    ingresses: BTreeSet<String>,
}
impl Options {
    pub(super) fn with_active(mut self, active: &[Value]) -> Result<Value> {
        for record in active {
            self.sources.insert(
                record["source_id"].as_str().unwrap_or("").into(),
                (
                    i64::MAX,
                    record["source_name"].as_str().unwrap_or("").into(),
                ),
            );
            self.models
                .insert(record["model"].as_str().unwrap_or("").into());
            self.ingresses
                .insert(record["ingress"].as_str().unwrap_or("unknown").into());
        }
        ensure!(
            self.sources.len() <= MAX_OPTIONS
                && self.models.len() <= MAX_OPTIONS
                && self.ingresses.len() <= MAX_OPTIONS,
            "筛选选项过多，请缩小查询范围"
        );
        Ok(
            json!({"sources":self.sources.into_iter().map(|(id,(_,name))|json!({"id":id,"name":name})).collect::<Vec<_>>(),"models":self.models,"ingresses":self.ingresses}),
        )
    }

    fn extend(&mut self, other: Self) {
        for (id, latest) in other.sources {
            match self.sources.entry(id) {
                Entry::Occupied(mut entry) if entry.get().0 < latest.0 => {
                    entry.insert(latest);
                }
                Entry::Vacant(entry) => {
                    entry.insert(latest);
                }
                _ => {}
            }
        }
        self.models.extend(other.models);
        self.ingresses.extend(other.ingresses);
    }
}

#[derive(Clone)]
pub(super) struct Snapshot {
    pub(super) groups: BTreeMap<(String, String), Group>,
    pub(super) daily: Vec<Value>,
    pub(super) options: Options,
    pub(super) performance: Value,
}

// Positive IEEE-754 bit patterns have the same ordering as their numeric values.
// Frequencies preserve exact percentiles without retaining every request sample.
const MAX_TIMING_VALUES: usize = 8192;

#[derive(Clone, Default)]
struct Timings {
    latency: BTreeMap<u64, u64>,
    first_token: BTreeMap<u64, u64>,
    exhausted: bool,
}
impl Timings {
    fn insert(&mut self, value: f64, first_token: bool) {
        if self.exhausted {
            return;
        }
        let key = if value == 0.0 { 0 } else { value.to_bits() };
        let distinct = self.latency.len() + self.first_token.len();
        let target = if first_token {
            &mut self.first_token
        } else {
            &mut self.latency
        };
        if !target.contains_key(&key) && distinct == MAX_TIMING_VALUES {
            self.latency.clear();
            self.first_token.clear();
            self.exhausted = true;
            return;
        }
        *target.entry(key).or_default() += 1;
    }

    fn performance(&self) -> Value {
        fn percentile(values: &BTreeMap<u64, u64>, percent: u64) -> Value {
            let count: u64 = values.values().sum();
            if count == 0 {
                return Value::Null;
            }
            let rank = (count - 1) * percent;
            let mut seen = 0;
            let mut low = 0.0;
            for (&bits, &frequency) in values {
                let end = seen + frequency;
                if seen <= rank / 100 && rank / 100 < end {
                    low = f64::from_bits(bits);
                }
                if rank.div_ceil(100) < end {
                    let high = f64::from_bits(bits);
                    return json!(low + (high - low) * (rank % 100) as f64 / 100.0);
                }
                seen = end;
            }
            unreachable!("percentile rank lies within sample count")
        }
        json!({"p50_latency_ms":percentile(&self.latency,50),"p95_latency_ms":percentile(&self.latency,95),
            "p50_first_token_ms":percentile(&self.first_token,50),"p95_first_token_ms":percentile(&self.first_token,95)})
    }
}

fn sql_performance(connection: &Connection, query: &Query) -> Result<Value> {
    let mut performance = json!({});
    for (name, expression, condition) in [
        ("latency_ms", "latency_ms", "latency_ms>0"),
        ("first_token_ms", "json_extract(metadata,'$.first_token_latency_ms')",
            "json_type(metadata,'$.first_token_latency_ms') IN ('integer','real') AND json_extract(metadata,'$.first_token_latency_ms')>=0"),
    ] {
        let percentile = |percent| format!(
            "MAX(CASE WHEN position=((count-1)*{percent})/100+1 THEN value END) + (MAX(CASE WHEN position=((count-1)*{percent}+99)/100+1 THEN value END)-MAX(CASE WHEN position=((count-1)*{percent})/100+1 THEN value END))*(((MAX(count)-1)*{percent})%100)/100.0"
        );
        let sql = format!("WITH attempts AS ({ATTEMPT_ROWS}), samples AS (SELECT {expression} AS value FROM attempts WHERE {FILTERS} AND status!='active' AND {condition}), ranked AS (SELECT value,ROW_NUMBER() OVER (ORDER BY value) AS position,COUNT(*) OVER () AS count FROM samples) SELECT {},{} FROM ranked", percentile(50), percentile(95));
        let (p50, p95) = connection.query_row(&sql, rusqlite::params_from_iter(query.bindings()), |row| {
            Ok((row.get::<_, Option<f64>>(0)?, row.get::<_, Option<f64>>(1)?))
        })?;
        performance[format!("p50_{name}")] = json!(p50);
        performance[format!("p95_{name}")] = json!(p95);
    }
    Ok(performance)
}

#[derive(Clone)]
pub(in crate::usage) struct Cached {
    query: Query,
    changes: u64,
    data_version: i64,
    cursor: i64,
    snapshot: Snapshot,
    timings: Timings,
}
impl Cached {
    fn offset(&self, query: &Query) -> Option<usize> {
        let old = &self.query;
        if old.source != query.source
            || old.model != query.model
            || old.ingress != query.ingress
            || old.status != query.status
            || old.search != query.search
        {
            return None;
        }
        let offset = query.days.binary_search(&old.from).ok()?;
        (query.days.get(offset..offset + old.days.len())? == old.days).then_some(offset)
    }
}

// Callers exclude mutations to already-cached rows before acknowledging writes.
// Unexpected writes retain their old change counter and are discarded on lookup.
pub(in crate::usage) fn preserve_after_write(
    data: &mut StoreData,
    before: u64,
    affected: Option<i64>,
) {
    let after = data.connection.total_changes();
    data.analytics_cache.retain_mut(|cached| {
        if cached.changes != before || affected.is_some_and(|attempt| attempt <= cached.cursor) {
            return false;
        }
        cached.changes = after;
        true
    });
}

fn zero_day() -> Value {
    let mut day = empty_totals();
    day["total_usd"] = json!(0);
    day
}

fn merge_totals(target: &mut Value, added: Value) {
    if added["requests"] == 0 {
        return;
    }
    if target["requests"] == 0 {
        *target = added;
        return;
    }
    for (field, value) in added.as_object().expect("aggregate object") {
        if !value.is_number() {
            continue;
        }
        let old = &target[field];
        target[field] = match (old.as_u64(), value.as_u64()) {
            (Some(a), Some(b)) => json!(a.saturating_add(b)),
            _ if old.is_null() => value.clone(),
            _ => json!(old.as_f64().unwrap_or(0.0) + value.as_f64().expect("numeric aggregate")),
        };
    }
}

fn load(
    connection: &Connection,
    query: &Query,
    base: Option<&Cached>,
    mut timings: Timings,
) -> Result<(Snapshot, Timings)> {
    let mut bindings = query.bindings();
    bindings.extend([
        SqlValue::Text(serde_json::to_string(&query.days)?),
        SqlValue::Integer(base.map_or(i64::MAX, |base| base.cursor)),
        SqlValue::Integer(base.map_or(query.from, |base| base.query.from) as i64),
        SqlValue::Integer(base.map_or(query.from, |base| base.query.to) as i64),
    ]);
    let attempts = format!("WITH {DELTA}, attempts AS ({ATTEMPT_ROWS})");
    let aggregate = aggregate_json();
    let mut groups = BTreeMap::new();
    let mut statement = connection.prepare(&format!("{attempts} SELECT source_id,source_name,model,MAX(attempt),{aggregate} FROM attempts WHERE {FILTERS} GROUP BY source_id,model ORDER BY source_id,model LIMIT {}", MAX_OPTIONS + 1))?;
    for row in statement.query_map(rusqlite::params_from_iter(&bindings), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, String>(4)?,
        ))
    })? {
        let (source, name, model, attempt, totals) = row?;
        let mut totals: Value = serde_json::from_str(&totals)?;
        totals["source_id"] = json!(source);
        totals["source_name"] = json!(name);
        totals["model"] = json!(model);
        groups.insert((source, model), Group { attempt, totals });
    }
    let mut daily = vec![zero_day(); query.days.len() - 1];
    let sql = format!("{attempts} SELECT CAST(days.key AS INTEGER),{aggregate} FROM json_each(?8) days JOIN attempts ON started_at>=days.value AND started_at<json_extract(?8,'$['||(CAST(days.key AS INTEGER)+1)||']') WHERE {FILTERS} GROUP BY days.key ORDER BY CAST(days.key AS INTEGER)");
    let mut statement = connection.prepare(&sql)?;
    for row in statement.query_map(rusqlite::params_from_iter(&bindings), |row| {
        Ok((row.get::<_, usize>(0)?, row.get::<_, String>(1)?))
    })? {
        let (index, totals) = row?;
        daily[index] = serde_json::from_str(&totals)?;
    }
    let mut sources = BTreeMap::new();
    let mut models = BTreeSet::new();
    let mut ingresses = BTreeSet::new();
    let mut statement = connection.prepare(&format!("{attempts} SELECT source_id,source_name,MAX(attempt) FROM attempts GROUP BY source_id ORDER BY source_id LIMIT {}", MAX_OPTIONS + 1))?;
    for row in statement.query_map(rusqlite::params_from_iter(&bindings), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(2)?,
            row.get::<_, String>(1)?,
        ))
    })? {
        let (id, attempt, name) = row?;
        sources.insert(id, (attempt, name));
    }
    for (column, target) in [("model", &mut models), ("ingress", &mut ingresses)] {
        let mut statement = connection.prepare(&format!(
            "{attempts} SELECT DISTINCT {column} FROM attempts ORDER BY {column} LIMIT {}",
            MAX_OPTIONS + 1
        ))?;
        for value in statement.query_map(rusqlite::params_from_iter(&bindings), |row| {
            row.get::<_, String>(0)
        })? {
            target.insert(value?);
        }
    }
    if !timings.exhausted {
        let sql = format!("{attempts} SELECT latency_ms,CASE WHEN json_type(metadata,'$.first_token_latency_ms') IN ('integer','real') AND json_extract(metadata,'$.first_token_latency_ms')>=0 THEN json_extract(metadata,'$.first_token_latency_ms') END FROM attempts WHERE {FILTERS} AND status!='active'");
        let mut statement = connection.prepare(&sql)?;
        for row in statement.query_map(rusqlite::params_from_iter(&bindings), |row| {
            Ok((row.get::<_, f64>(0)?, row.get::<_, Option<f64>>(1)?))
        })? {
            let (latency, first_token) = row?;
            if latency > 0.0 {
                timings.insert(latency, false);
            }
            if let Some(value) = first_token {
                timings.insert(value, true);
            }
            if timings.exhausted {
                break;
            }
        }
    }
    Ok((
        Snapshot {
            groups,
            daily,
            options: Options {
                sources,
                models,
                ingresses,
            },
            performance: Value::Null,
        },
        timings,
    ))
}

pub(super) fn snapshot(data: &mut StoreData, query: &Query) -> Result<Snapshot> {
    let transaction = data.connection.unchecked_transaction()?;
    let changes = transaction.total_changes();
    let data_version = transaction.pragma_query_value(None, "data_version", |row| row.get(0))?;
    let cursor =
        transaction.query_row("SELECT COALESCE(MAX(attempt),0) FROM usage", [], |row| {
            row.get(0)
        })?;
    data.analytics_cache
        .retain(|cached| cached.changes == changes && cached.data_version == data_version);
    let latest = data
        .analytics_cache
        .iter()
        .enumerate()
        .filter_map(|(index, cached)| {
            if cached.cursor > cursor {
                return None;
            }
            cached.offset(query).map(|offset| {
                (
                    cached.query.days == query.days,
                    cached.cursor,
                    index,
                    offset,
                )
            })
        })
        .max();
    let mut cached = latest.map(|(exact, _, index, offset)| {
        let base = if exact {
            data.analytics_cache.remove(index).expect("cached query")
        } else {
            data.analytics_cache[index].clone()
        };
        (base, offset)
    });
    if cached
        .as_ref()
        .is_none_or(|(cached, _)| cached.cursor != cursor || cached.query.days != query.days)
    {
        let timings = cached
            .as_mut()
            .map(|(base, _)| std::mem::take(&mut base.timings))
            .unwrap_or_default();
        let (added, timings) = load(
            &transaction,
            query,
            cached.as_ref().map(|(cached, _)| cached),
            timings,
        )?;
        if let Some((base, offset)) = &mut cached {
            if *offset > 0 {
                let previous = std::mem::replace(
                    &mut base.snapshot.daily,
                    vec![zero_day(); query.days.len() - 1],
                );
                for (index, day) in previous.into_iter().enumerate() {
                    base.snapshot.daily[*offset + index] = day;
                }
            } else {
                base.snapshot
                    .daily
                    .resize_with(query.days.len() - 1, zero_day);
            }
            for (key, group) in added.groups {
                match base.snapshot.groups.entry(key) {
                    Entry::Vacant(entry) => {
                        entry.insert(group);
                    }
                    Entry::Occupied(mut entry) => {
                        let target = entry.get_mut();
                        if group.attempt > target.attempt {
                            target.attempt = group.attempt;
                            target.totals["source_name"] = group.totals["source_name"].clone();
                        }
                        merge_totals(&mut target.totals, group.totals);
                    }
                }
            }
            for (day, added) in base.snapshot.daily.iter_mut().zip(added.daily) {
                merge_totals(day, added);
            }
            base.snapshot.options.extend(added.options);
            base.timings = timings;
        } else {
            cached = Some((
                Cached {
                    query: query.clone(),
                    changes,
                    data_version,
                    cursor,
                    snapshot: added,
                    timings,
                },
                0,
            ));
        }
        let base = &mut cached.as_mut().expect("loaded snapshot").0;
        base.query = query.clone();
        base.cursor = cursor;
        base.snapshot.performance = if base.timings.exhausted {
            sql_performance(&transaction, query)?
        } else {
            base.timings.performance()
        };
    }
    transaction.commit()?;
    let cached = cached.expect("loaded snapshot").0;
    ensure!(
        cached.snapshot.groups.len() <= MAX_OPTIONS,
        "统计分组过多，请缩小查询范围"
    );
    ensure!(
        cached.snapshot.options.sources.len() <= MAX_OPTIONS
            && cached.snapshot.options.models.len() <= MAX_OPTIONS
            && cached.snapshot.options.ingresses.len() <= MAX_OPTIONS,
        "筛选选项过多，请缩小查询范围"
    );
    let result = cached.snapshot.clone();
    if data.analytics_cache.len() == 8 {
        data.analytics_cache.pop_front();
    }
    data.analytics_cache.push_back(cached);
    Ok(result)
}
