//! Posting: the one place an entry is made.
//!
//! An entry is validated, numbered, hashed and written in one transaction, together with the two
//! counters it moves, and never changed again. The lines live inside the entry (the hash covers them)
//! and are copied into `gl_line` rows for queries; if that copy fails the entry still stands and the
//! tick makes the rows. A correction is a reversing entry that points at the original.

use aether_sdk::dates::{format_date, parse_date, Datelike};
use aether_sdk::db::Filter;
use aether_sdk::decimal::Decimal;
use aether_sdk::events::Received;
use aether_sdk::prelude::*;

use crate::common::{
    account_by_code, actor_name, id_of, journal_by_code, ledger, require, require_accountant, text, today_date, Record,
};
use crate::rules::{apply_template, canonical_for, check_lines, idempotency_key, line_json, merge_lines, numbered, period_for, sha256_hex, Facts, Line, GENESIS, HASH_VERSION};

/// The document an entry was made from.
#[derive(Debug, Clone)]
pub struct Source {
    pub kind: String,
    pub id: String,
    /// The document's own version, from the payload; none when the rule names no version path.
    pub version: Option<i64>,
    /// The posting rule's version, kept as information.
    pub rule_version: i64,
    pub key: String,
}

pub struct Posting {
    pub journal: String,
    pub date: String,
    pub memo: Option<String>,
    pub currency: Option<String>,
    pub lines: Vec<Line>,
    pub source: Option<Source>,
    pub reverses: Option<String>,
    pub reason: Option<String>,
}

#[derive(Deserialize)]
struct LineInput {
    account: String,
    #[serde(default)]
    debit: Option<Decimal>,
    #[serde(default)]
    credit: Option<Decimal>,
    #[serde(default)]
    party: Option<String>,
    #[serde(default)]
    tax_code: Option<String>,
    #[serde(default)]
    memo: Option<String>,
}

#[derive(Deserialize)]
struct PostInput {
    journal: String,
    date: String,
    #[serde(default)]
    memo: Option<String>,
    #[serde(default)]
    currency: Option<String>,
    lines: Vec<LineInput>,
}

#[derive(Deserialize)]
struct Reverse {
    id: String,
    reason: String,
    #[serde(default)]
    date: Option<String>,
}

#[derive(Deserialize)]
struct Id {
    id: String,
}

fn to_line(input: LineInput) -> Result<Line> {
    let zero = Decimal::zero(2);
    Ok(Line {
        account: input.account,
        debit: input.debit.unwrap_or(zero).with_scale(2)?,
        credit: input.credit.unwrap_or(zero).with_scale(2)?,
        party: input.party.filter(|p| !p.is_empty()),
        tax_code: input.tax_code.filter(|p| !p.is_empty()),
        memo: input.memo.filter(|p| !p.is_empty()),
    })
}

/// The id of each line's account, checked: it exists, is active, is not a group, and has a party when it is a control account.
fn resolve_accounts(lines: &[Line]) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let account = account_by_code(&line.account)?;
        let number = index + 1;
        if account.get("is_active") == Some(&json!(false)) {
            return Err(Error::msg(format!("line {number}: account `{}` is not in use any more", line.account)));
        }
        if account.get("is_group") == Some(&json!(true)) {
            return Err(Error::msg(format!("line {number}: `{}` is a group: post to an account under it", line.account)));
        }
        if account.get("is_control") == Some(&json!(true)) && line.party.is_none() {
            return Err(Error::msg(format!("line {number}: `{}` needs a party (who it is owed to or by)", line.account)));
        }
        ids.push(id_of(&account)?.to_string());
    }
    Ok(ids)
}

fn open_period(date: aether_sdk::dates::NaiveDate) -> Result<String> {
    let rows: Vec<Record> = db::find("gl_period").order_by("start_date").limit(1000).all()?;
    let mut periods = Vec::new();
    for row in &rows {
        periods.push((parse_date(text(row, "start_date").unwrap_or_default())?, parse_date(text(row, "end_date").unwrap_or_default())?, row));
    }
    let period = period_for(date, &periods).ok_or_else(|| Error::msg(format!("there is no period for {date}: an administrator creates the fiscal year")))?;
    if text(period, "status") != Some("open") {
        return Err(Error::msg(format!("the period {} is closed: post the correction in an open period, naming the entry it corrects", text(period, "name").unwrap_or("?"))));
    }
    Ok(id_of(period)?.to_string())
}

/// The counter of a journal for a year, made on first use: every journal numbers every year from 1. Two first
/// uses at once make the same unique key; the loser reads the winner's row.
fn year_counter(journal_id: &str, year: i32) -> Result<Record> {
    let key = format!("{journal_id}:{year}");
    if let Some(row) = db::find::<Record>("gl_counter").filter("key", key.as_str()).first()? {
        return Ok(row);
    }
    match db::create::<Record>("gl_counter", &json!({ "key": key, "next": 0 })) {
        Ok(row) => Ok(row),
        Err(error) => db::find::<Record>("gl_counter").filter("key", key.as_str()).first()?.ok_or(error),
    }
}

pub fn post(posting: Posting) -> Result<Record> {
    let ledger_row = ledger()?;
    let base = text(&ledger_row, "base_currency").unwrap_or_default().to_string();
    let currency = posting.currency.clone().unwrap_or_else(|| base.clone());
    if currency != base {
        return Err(Error::msg(format!("this ledger keeps {base}: entries in {currency} are not supported yet, convert first")));
    }
    let date = parse_date(&posting.date)?;
    if let Some(source) = &posting.source {
        if let Some(existing) = db::find::<Record>("gl_entry").filter("idem_key", source.key.as_str()).first()? {
            return Ok(existing);
        }
    }
    let journal = journal_by_code(&posting.journal)?;
    if journal.get("is_active") == Some(&json!(false)) {
        return Err(Error::msg("this journal is not in use"));
    }
    let lines = merge_lines(posting.lines.clone());
    let total = check_lines(&lines)?;
    let account_ids = resolve_accounts(&lines)?;
    let period = open_period(date)?;
    let stored: Vec<Value> = lines
        .iter()
        .zip(&account_ids)
        .map(|(line, id)| {
            let mut value = line_json(&line.account, line);
            value["account_id"] = json!(id);
            value
        })
        .collect();
    let pairs: Vec<(String, &Line)> = lines.iter().map(|l| (l.account.clone(), l)).collect();

    let mut last_error: Option<Error> = None;
    for _attempt in 0..4 {
        let ledger_row = ledger()?;
        let journal = journal_by_code(&posting.journal)?;
        let last: Option<Record> = db::find("gl_entry").order_by("-seq").first()?;
        let prev_hash = last.as_ref().and_then(|e| text(e, "hash")).unwrap_or(GENESIS).to_string();
        let seq = ledger_row.get("seq").and_then(Value::as_i64).unwrap_or(0) + 1;
        let counter = year_counter(id_of(&journal)?, date.year())?;
        let number = numbered(text(&journal, "prefix").unwrap_or("JE"), date.year(), counter.get("next").and_then(Value::as_i64).unwrap_or(0) + 1);
        let journal_code = text(&journal, "code").unwrap_or(posting.journal.as_str()).to_string();
        let date_text = format_date(date);
        let facts = Facts {
            prev_hash: &prev_hash,
            seq,
            number: &number,
            date: &date_text,
            journal_code: &journal_code,
            currency: &currency,
            reverses: posting.reverses.as_deref(),
            source_kind: posting.source.as_ref().map(|s| s.kind.as_str()),
            source_id: posting.source.as_ref().map(|s| s.id.as_str()),
            source_version: posting.source.as_ref().and_then(|s| s.version),
            reason: posting.reason.as_deref(),
            memo: posting.memo.as_deref(),
        };
        let hash = sha256_hex(&canonical_for(HASH_VERSION, &facts, &pairs));
        let idem = posting.source.as_ref().map(|s| s.key.clone()).unwrap_or_else(|| format!("manual:{number}"));
        let mut data = json!({
            "number": number, "seq": seq, "journal": journal["id"], "period": period, "date": format_date(date), "currency": currency,
            "total": total, "lines": stored, "line_count": stored.len(), "idem_key": idem, "prev_hash": prev_hash, "hash": hash,
            "hash_version": HASH_VERSION, "journal_code": journal_code, "posted_at": context::current()?.now,
        });
        if let Some(memo) = &posting.memo {
            data["memo"] = json!(memo);
        }
        if let Some(source) = &posting.source {
            data["source_kind"] = json!(source.kind);
            data["source_id"] = json!(source.id);
            data["rule_version"] = json!(source.rule_version);
            if let Some(version) = source.version {
                data["source_version"] = json!(version);
            }
        }
        if let Some(original) = &posting.reverses {
            data["reverses"] = json!(original);
        }
        if let Some(reason) = &posting.reason {
            data["reason"] = json!(reason);
        }
        if let Some(who) = actor_name()? {
            data["posted_by"] = json!(who);
        }
        match db::transaction()
            .increment("gl_ledger", id_of(&ledger_row)?, "seq", 1.0)
            .increment("gl_counter", id_of(&counter)?, "next", 1.0)
            .create("gl_entry", &data)
            .run()
        {
            Ok(done) => {
                let entry: Record = done.into_iter().last().and_then(|v| serde_json::from_value(v).ok()).ok_or_else(|| Error::msg("the entry was not returned"))?;
                if let Err(error) = index_lines(&entry) {
                    return Err(Error::msg(format!("entry {} is posted, but its lines are not indexed yet ({error}); the next tick completes it", text(&entry, "number").unwrap_or("?"))));
                }
                return Ok(entry);
            }
            Err(error) => {
                // Someone else took the number or the same source: look again before trying again.
                if let Some(source) = &posting.source {
                    if let Some(existing) = db::find::<Record>("gl_entry").filter("idem_key", source.key.as_str()).first()? {
                        return Ok(existing);
                    }
                }
                last_error = Some(error);
            }
        }
    }
    Err(last_error.unwrap_or_else(|| Error::msg("could not post the entry")))
}

/// Make the `gl_line` rows of an entry, if there are none yet.
pub fn index_lines(entry: &Record) -> Result<()> {
    let entry_id = id_of(entry)?;
    if db::count("gl_line", Filter::eq("entry", entry_id))? > 0 {
        return Ok(());
    }
    let lines = entry["lines"].as_array().ok_or_else(|| Error::msg("an entry has no lines"))?;
    let mut tx = db::transaction();
    for (index, line) in lines.iter().enumerate() {
        let mut row = json!({
            "entry": entry_id, "line_no": index + 1, "account": line["account_id"], "debit": line["debit"], "credit": line["credit"], "date": entry["date"],
        });
        for field in ["party", "tax_code", "memo"] {
            if !line[field].is_null() {
                row[field] = line[field].clone();
            }
        }
        tx = tx.create("gl_line", &row);
    }
    tx.run()?;
    Ok(())
}

fn post_manual(input: PostInput) -> Result<Record> {
    require_accountant()?;
    let lines = input.lines.into_iter().map(to_line).collect::<Result<Vec<_>>>()?;
    post(Posting { journal: input.journal, date: input.date, memo: input.memo, currency: input.currency, lines, source: None, reverses: None, reason: None })
}

fn reverse(input: Reverse) -> Result<Record> {
    require_accountant()?;
    let original = require("gl_entry", &input.id, "entry")?;
    if input.reason.trim().is_empty() {
        return Err(Error::msg("say why the entry is reversed"));
    }
    if !original.get("reverses").is_none_or(Value::is_null) {
        return Err(Error::msg("this entry is itself a reversal: post the correct entry instead"));
    }
    if db::count("gl_entry", Filter::eq("reverses", input.id.as_str()))? > 0 {
        return Err(Error::msg("this entry is already reversed"));
    }
    let line_ids: Vec<String> = db::find::<Record>("gl_line").filter("entry", input.id.as_str()).limit(100).all()?.iter().filter_map(|l| text(l, "id").map(str::to_string)).collect();
    if !line_ids.is_empty() && db::count("gl_allocation", Filter::one_of("debit_line", line_ids.clone()).or(Filter::one_of("credit_line", line_ids)))? > 0 {
        return Err(Error::msg("some of its lines are matched against others: undo those allocations first"));
    }
    let journal = require("gl_journal", text(&original, "journal").unwrap_or_default(), "journal")?;
    let mut lines = Vec::new();
    for line in original["lines"].as_array().ok_or_else(|| Error::msg("an entry has no lines"))? {
        let side = |name: &str| -> Result<Decimal> { serde_json::from_value(line[name].clone()).map_err(|e| Error::msg(format!("{name}: {e}"))) };
        lines.push(Line {
            account: line["account"].as_str().unwrap_or_default().to_string(),
            debit: side("credit")?,
            credit: side("debit")?,
            party: line["party"].as_str().map(str::to_string),
            tax_code: line["tax_code"].as_str().map(str::to_string),
            memo: line["memo"].as_str().map(str::to_string),
        });
    }
    let date = match &input.date {
        Some(date) => format_date(parse_date(date)?),
        None => format_date(today_date()?),
    };
    post(Posting {
        journal: text(&journal, "code").unwrap_or_default().to_string(),
        date,
        memo: Some(format!("Reversal of {}", text(&original, "number").unwrap_or("?"))),
        currency: text(&original, "currency").map(str::to_string),
        lines,
        source: None,
        reverses: Some(input.id),
        reason: Some(input.reason),
    })
}

/// A whole number in a payload (a document's version), sent as a number or as text.
fn whole_number(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64(),
        Value::String(t) => t.trim().parse().ok(),
        _ => None,
    }
}

/// An event with a posting rule becomes an entry; the same document at the same version again is the same entry.
///
/// The rule says where the document, its version and its date are in the payload. A rule without `version_path`
/// keeps the old key (the rule's version); a rule with one posts a changed document again and never posts a
/// redelivered one twice, whichever rule version is current. A rule without `date_path` is refused: the entry date
/// is the document's, and "the day it was processed" has to be chosen on purpose (`date_path` of `"@today"`).
fn from_event(received: &Received) -> Result<Value> {
    // Events are delivered by the kernel (the forwarder's call has no member behind it). A person, even an
    // administrator, calling this directly could post whatever they like and take the key of a real document.
    let context = context::current()?;
    if context.is_member() {
        return Err(Error::msg("events are delivered by the kernel, not posted by hand: use post_entry"));
    }
    let rule = db::find::<Record>("gl_posting_rule").filter("event", received.event.as_str()).filter("is_active", true).order_by("-version").first()?;
    // An event nobody has a rule for fails, so the delivery job stays visible and is retried once the rule exists.
    let Some(rule) = rule else {
        return Err(Error::msg(format!("no active posting rule for `{}`: add one and the delivery is retried", received.event)));
    };
    let payload = &received.payload;
    let ledger_row = ledger()?;
    if let Some(path) = text(&rule, "currency_path") {
        let currency = crate::rules::dig(payload, path).and_then(Value::as_str).unwrap_or_default();
        if currency != text(&ledger_row, "base_currency").unwrap_or_default() {
            return Err(Error::msg(format!("the event is in `{currency}` but the ledger keeps {}: multi-currency posting is not supported yet", text(&ledger_row, "base_currency").unwrap_or("?"))));
        }
    }
    let source_id = crate::rules::dig(payload, text(&rule, "source_path").unwrap_or_default())
        .and_then(Value::as_str)
        .ok_or_else(|| Error::msg("the event does not name its document where the rule looks"))?
        .to_string();
    let version = match text(&rule, "version_path") {
        Some(path) => Some(
            crate::rules::dig(payload, path)
                .and_then(whole_number)
                .ok_or_else(|| Error::msg(format!("the rule wants the document's version at `{path}` and the event has none")))?,
        ),
        None => None,
    };
    let date = match text(&rule, "date_path") {
        Some("@today") => today_date()?,
        Some(path) => {
            let raw = crate::rules::dig(payload, path).and_then(Value::as_str).ok_or_else(|| Error::msg(format!("the rule dates the entry from `{path}` and the event has no date there")))?;
            parse_date(raw.get(..10).unwrap_or(raw))?
        }
        None => return Err(Error::msg("the posting rule does not say which date to use: set `date_path` to the date in the payload, or to `@today` on purpose")),
    };
    let resolved = apply_template(&rule["lines"], payload)?;
    let zero = Decimal::zero(2);
    let lines: Vec<Line> = resolved
        .into_iter()
        .map(|r| Line { account: r.account, debit: if r.debit { r.amount } else { zero }, credit: if r.debit { zero } else { r.amount }, party: r.party, tax_code: None, memo: None })
        .collect();
    let journal = require("gl_journal", text(&rule, "journal").unwrap_or_default(), "journal")?;
    let rule_version = rule.get("version").and_then(Value::as_i64).unwrap_or(1);
    let key = idempotency_key(&received.event, &source_id, version.map(|v| v.to_string()).as_deref(), rule_version);
    let entry = post(Posting {
        journal: text(&journal, "code").unwrap_or_default().to_string(),
        date: format_date(date),
        memo: Some(text(&rule, "memo").map_or_else(|| received.event.clone(), str::to_string)),
        currency: None,
        lines,
        source: Some(Source { kind: received.event.clone(), id: source_id, version, rule_version, key }),
        reverses: None,
        reason: None,
    })?;
    Ok(json!({ "posted": true, "entry": entry["number"] }))
}

handler! {
    /// Post an entry by hand.
    fn post_entry(input: PostInput) -> Record {
        post_manual(input)
    }

    /// Correct an entry by posting its mirror image.
    fn reverse_entry(input: Reverse) -> Record {
        reverse(input)
    }

    fn get_entry(input: Id) -> Option<Record> {
        crate::common::require_reader()?;
        db::get("gl_entry", &input.id)
    }

    /// Runs for the events the manifest listens to: turns them into entries by their posting rule.
    fn on_posting_event(input: Received) -> Value {
        from_event(&input)
    }

    /// Complete the line rows of entries whose copy failed. Walks forward from the entry after the last one known to
    /// be indexed (`gl_ledger.indexed_seq`), so an old gap is found however many entries came after it.
    fn index_recent_entries(_: Empty) -> Value {
        let ledger_row = ledger()?;
        let from = ledger_row.get("indexed_seq").and_then(Value::as_i64).unwrap_or(0);
        let batch: Vec<Record> = db::find("gl_entry").matching(Filter::gt("seq", from)).order_by("seq").limit(100).all()?;
        let (mut repaired, mut through) = (0u64, from);
        for entry in &batch {
            if db::count("gl_line", Filter::eq("entry", id_of(entry)?))? == 0 {
                index_lines(entry)?;
                repaired += 1;
            }
            through = entry.get("seq").and_then(Value::as_i64).unwrap_or(through);
        }
        if through > from {
            db::update::<Record>("gl_ledger", id_of(&ledger_row)?, &json!({ "indexed_seq": through }))?;
        }
        Ok(json!({ "repaired": repaired, "indexed_seq": through, "more": batch.len() == 100 }))
    }
}

// Only for a one-off check that the rules refuse to change or remove a posted entry:
// `cargo build --features tamper_test`. Never in the published plugin.
#[cfg(feature = "tamper_test")]
handler! {
    fn tamper_test(input: Id) -> Value {
        let change = db::update::<Record>("gl_entry", &input.id, &json!({ "memo": "changed" })).map(|_| "allowed").unwrap_or_else(|e| { let _ = e; "refused" });
        let remove = db::delete::<Record>("gl_entry", &input.id).map(|_| "allowed").unwrap_or("refused");
        Ok(json!({ "update": change, "delete": remove }))
    }
}
