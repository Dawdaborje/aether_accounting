//! Reports over the immutable lines: trial balance, general ledger, journal listing, chain check.

use std::collections::BTreeMap;

use aether_sdk::dates::format_date;
use aether_sdk::db::{Figure, Filter};
use aether_sdk::decimal::Decimal;
use aether_sdk::prelude::*;

use crate::common::{account_by_code, amount_of, dec, id_of, journal_by_code, require_reader, text, today_date, Record};
use crate::rules::{canonical, sha256_hex, Line, GENESIS};

#[derive(Deserialize, Default)]
struct Range {
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
}

#[derive(Deserialize)]
struct Ledger {
    account: String,
    #[serde(default)]
    party: Option<String>,
    #[serde(flatten)]
    range: Range,
}

#[derive(Deserialize)]
struct Listing {
    journal: String,
    #[serde(flatten)]
    range: Range,
}

#[derive(Deserialize, Default)]
struct Verify {
    #[serde(default)]
    after_seq: Option<i64>,
    #[serde(default)]
    limit: Option<u32>,
}

fn zero() -> Decimal {
    Decimal::zero(2)
}

fn date_filter(range: &Range) -> Result<Filter> {
    let mut filter = Filter::all();
    if let Some(from) = &range.from {
        filter = filter.and(Filter::gte("date", format_date(aether_sdk::dates::parse_date(from)?)));
    }
    if let Some(to) = &range.to {
        filter = filter.and(Filter::lte("date", format_date(aether_sdk::dates::parse_date(to)?)));
    }
    Ok(filter)
}

fn trial_balance(range: Range) -> Result<Value> {
    require_reader()?;
    let rows = db::find::<Record>("gl_line")
        .matching(date_filter(&range)?)
        .aggregate(&["account"], &[("debit", Figure::sum("debit")), ("credit", Figure::sum("credit"))])?;
    let accounts: Vec<Record> = db::find("gl_account").order_by("code").limit(1000).all()?;
    let mut by_id: BTreeMap<String, (Decimal, Decimal)> = BTreeMap::new();
    for row in &rows {
        let id = row["account"].as_str().unwrap_or_default().to_string();
        let entry = by_id.entry(id).or_insert((zero(), zero()));
        entry.0 = entry.0 + amount_of(&row["debit"])?;
        entry.1 = entry.1 + amount_of(&row["credit"])?;
    }
    // Groups show the sum of everything under them.
    let parent_of: BTreeMap<String, String> = accounts.iter().filter_map(|a| Some((text(a, "id")?.to_string(), text(a, "parent")?.to_string()))).collect();
    let mut rolled = by_id.clone();
    for (id, (debit, credit)) in &by_id {
        let mut current = parent_of.get(id);
        let mut steps = 0;
        while let Some(parent) = current {
            let entry = rolled.entry(parent.clone()).or_insert((zero(), zero()));
            entry.0 = entry.0 + *debit;
            entry.1 = entry.1 + *credit;
            current = parent_of.get(parent);
            steps += 1;
            if steps > 20 {
                break;
            }
        }
    }
    let (mut total_debit, mut total_credit) = (zero(), zero());
    let mut out = Vec::new();
    for account in &accounts {
        let id = id_of(account)?;
        let Some((debit, credit)) = rolled.get(id) else { continue };
        if account.get("is_group") != Some(&json!(true)) {
            total_debit = total_debit + *debit;
            total_credit = total_credit + *credit;
        }
        out.push(json!({
            "code": account["code"], "name": account["name"], "kind": account["kind"], "is_group": account["is_group"],
            "debit": debit, "credit": credit, "balance": *debit - *credit,
        }));
    }
    Ok(json!({ "rows": out, "total_debit": total_debit, "total_credit": total_credit, "balanced": total_debit == total_credit }))
}

fn general_ledger(input: Ledger) -> Result<Value> {
    require_reader()?;
    let account = account_by_code(&input.account)?;
    let account_id = id_of(&account)?;
    let mut base = Filter::eq("account", account_id);
    if let Some(party) = &input.party {
        base = base.and(Filter::eq("party", party.as_str()));
    }
    // What stood before the first day shown.
    let mut opening = zero();
    if let Some(from) = &input.range.from {
        let before = db::find::<Record>("gl_line")
            .matching(base.clone().and(Filter::lt("date", format_date(aether_sdk::dates::parse_date(from)?))))
            .aggregate(&[], &[("debit", Figure::sum("debit")), ("credit", Figure::sum("credit"))])?;
        if let Some(row) = before.first() {
            opening = amount_of(&row["debit"])? - amount_of(&row["credit"])?;
        }
    }
    let lines: Vec<Record> = db::find::<Record>("gl_line").matching(base.and(date_filter(&input.range)?)).order_by("date").limit(1000).all()?;
    let entry_ids: Vec<String> = lines.iter().filter_map(|l| text(l, "entry").map(str::to_string)).collect();
    let mut numbers: BTreeMap<String, (String, i64)> = BTreeMap::new();
    for chunk in entry_ids.chunks(100) {
        for entry in db::find::<Record>("gl_entry").matching(Filter::one_of("id", chunk.to_vec())).limit(100).all()? {
            numbers.insert(id_of(&entry)?.to_string(), (text(&entry, "number").unwrap_or("").to_string(), entry.get("seq").and_then(Value::as_i64).unwrap_or(0)));
        }
    }
    let mut ordered: Vec<(&Record, (String, i64))> = lines.iter().map(|l| (l, numbers.get(text(l, "entry").unwrap_or_default()).cloned().unwrap_or_default())).collect();
    ordered.sort_by(|a, b| (text(a.0, "date"), a.1 .1, a.0.get("line_no").and_then(Value::as_i64)).cmp(&(text(b.0, "date"), b.1 .1, b.0.get("line_no").and_then(Value::as_i64))));
    let mut running = opening;
    let mut rows = Vec::new();
    for (line, (number, _)) in ordered {
        running = running + dec(line, "debit")? - dec(line, "credit")?;
        rows.push(json!({
            "date": line["date"], "entry": number, "memo": line["memo"], "party": line["party"],
            "debit": line["debit"], "credit": line["credit"], "balance": running,
        }));
    }
    Ok(json!({ "account": account["code"], "opening": opening, "rows": rows, "closing": running }))
}

fn journal_listing(input: Listing) -> Result<Vec<Record>> {
    require_reader()?;
    let journal = journal_by_code(&input.journal)?;
    db::find::<Record>("gl_entry").matching(Filter::eq("journal", id_of(&journal)?).and(date_filter(&input.range)?)).order_by("seq").limit(500).all()
}

fn line_of(value: &Value) -> Result<Line> {
    let side = |name: &str| -> Result<Decimal> { serde_json::from_value(value[name].clone()).map_err(|e| Error::msg(format!("{name}: {e}"))) };
    Ok(Line {
        account: value["account"].as_str().unwrap_or_default().to_string(),
        debit: side("debit")?,
        credit: side("credit")?,
        party: value["party"].as_str().map(str::to_string),
        tax_code: value["tax_code"].as_str().map(str::to_string),
        memo: value["memo"].as_str().map(str::to_string),
    })
}

/// Replay the hash chain from the start (or after a sequence number) and report the first thing wrong.
fn verify(input: Verify) -> Result<Value> {
    require_reader()?;
    let after = input.after_seq.unwrap_or(0);
    let entries: Vec<Record> = db::find::<Record>("gl_entry").matching(Filter::gt("seq", after)).order_by("seq").limit(input.limit.unwrap_or(500).min(1000)).all()?;
    let journals: Vec<Record> = db::find("gl_journal").limit(200).all()?;
    let mut prev_hash = if after == 0 {
        GENESIS.to_string()
    } else {
        db::find::<Record>("gl_entry").filter("seq", after).first()?.and_then(|e| text(&e, "hash").map(str::to_string)).ok_or_else(|| Error::msg("there is no entry at that sequence number"))?
    };
    let mut expected_seq = after + 1;
    let mut problem: Option<String> = None;
    let mut unindexed = 0u64;
    for entry in &entries {
        let seq = entry.get("seq").and_then(Value::as_i64).unwrap_or(0);
        let number = text(entry, "number").unwrap_or("?");
        if seq != expected_seq {
            problem = Some(format!("a gap: expected entry {expected_seq}, found {seq} ({number})"));
            break;
        }
        if text(entry, "prev_hash") != Some(prev_hash.as_str()) {
            problem = Some(format!("{number} does not follow the entry before it"));
            break;
        }
        let journal_code = journals.iter().find(|j| text(j, "id") == text(entry, "journal")).and_then(|j| text(j, "code")).unwrap_or("?");
        let lines: Vec<Line> = entry["lines"].as_array().ok_or_else(|| Error::msg("an entry has no lines"))?.iter().map(line_of).collect::<Result<_>>()?;
        let pairs: Vec<(String, &Line)> = lines.iter().map(|l| (l.account.clone(), l)).collect();
        let recomputed = sha256_hex(&canonical(&prev_hash, seq, number, text(entry, "date").unwrap_or_default(), journal_code, text(entry, "currency").unwrap_or_default(), &pairs));
        if Some(recomputed.as_str()) != text(entry, "hash") {
            problem = Some(format!("{number} was changed after it was posted"));
            break;
        }
        if db::count("gl_line", Filter::eq("entry", id_of(entry)?))? as i64 != entry.get("line_count").and_then(Value::as_i64).unwrap_or(0) {
            unindexed += 1;
        }
        prev_hash = recomputed;
        expected_seq += 1;
    }
    Ok(json!({
        "checked": entries.len(), "ok": problem.is_none(), "problem": problem, "last_seq": expected_seq - 1, "last_hash": prev_hash,
        "entries_without_line_rows": unindexed, "as_of": format_date(today_date()?),
    }))
}

handler! {
    fn trial_balance_report(input: Option<Range>) -> Value {
        trial_balance(input.unwrap_or_default())
    }

    fn general_ledger_report(input: Ledger) -> Value {
        general_ledger(input)
    }

    fn journal_listing_report(input: Listing) -> Vec<Record> {
        journal_listing(input)
    }

    fn verify_chain(input: Option<Verify>) -> Value {
        verify(input.unwrap_or_default())
    }
}
