//! Allocations: matching a credit against a debit on a control account (what an employee is owed
//! against what was paid them). Written once; undoing one writes a negative row.

use aether_sdk::dates::format_date;
use aether_sdk::db::Filter;
use aether_sdk::decimal::Decimal;
use aether_sdk::prelude::*;

use crate::common::{account_by_code, actor_name, dec, id_of, require, require_accountant, require_reader, text, today_date, Record};
use crate::rules::{fifo, open_amount};

#[derive(Deserialize)]
struct Allocate {
    debit_line: String,
    credit_line: String,
    #[serde(default)]
    amount: Option<Decimal>,
}

#[derive(Deserialize)]
struct Id {
    id: String,
}

#[derive(Deserialize)]
struct Items {
    account: String,
    #[serde(default)]
    party: Option<String>,
}

#[derive(Deserialize)]
struct Lines {
    lines: Vec<String>,
}

#[derive(Deserialize)]
struct BySource {
    source_kind: String,
    source_id: String,
    account: String,
    #[serde(default)]
    party: Option<String>,
}

/// Lines asked about in one call.
const DETAIL_PAGE: usize = 200;

/// Lines looked at for open items in one call.
const OPEN_PAGE: u32 = 1000;

fn zero() -> Decimal {
    Decimal::zero(2)
}

/// What is left of a line after its allocations.
fn open_of(line: &Record, debit_side: bool) -> Result<Decimal> {
    let field = if debit_side { "debit" } else { "credit" };
    let link = if debit_side { "debit_line" } else { "credit_line" };
    let rows: Vec<Record> = db::find("gl_allocation").filter(link, id_of(line)?).limit(1000).all()?;
    let mut amounts = Vec::new();
    for row in &rows {
        amounts.push(dec(row, "amount")?);
    }
    Ok(open_amount(dec(line, field)?, &amounts))
}

/// The next place on a line's allocation list: the rows already written for it (an undoing is a row too) plus one.
/// `debit_slot` and `credit_slot` are unique, so two allocations that read the same count cannot both be written:
/// the second is refused and tries again from fresh figures, and then finds the room taken.
fn next_slot(line_id: &str, side_link: &str) -> Result<i64> {
    Ok(db::count("gl_allocation", Filter::eq(side_link, line_id))? as i64 + 1)
}

fn slot(line_id: &str, n: i64) -> String {
    format!("{line_id}#{n}")
}

fn allocate(input: Allocate) -> Result<Record> {
    require_accountant()?;
    let debit = require("gl_line", &input.debit_line, "debit line")?;
    let credit = require("gl_line", &input.credit_line, "credit line")?;
    if dec(&debit, "debit")? <= zero() {
        return Err(Error::msg("the first line is not a debit"));
    }
    if dec(&credit, "credit")? <= zero() {
        return Err(Error::msg("the second line is not a credit"));
    }
    if text(&debit, "account") != text(&credit, "account") {
        return Err(Error::msg("both lines are on the same account"));
    }
    let party = text(&debit, "party").filter(|p| Some(*p) == text(&credit, "party")).ok_or_else(|| Error::msg("both lines are for the same party"))?;
    let account = require("gl_account", text(&debit, "account").unwrap_or_default(), "account")?;
    if account.get("is_control") != Some(&json!(true)) {
        return Err(Error::msg("only a control account is matched"));
    }
    let mut last_error: Option<Error> = None;
    for _attempt in 0..4 {
        // The places are taken before the room is looked at: an allocation written after this count has the same
        // place and is refused, and one written before it is already in the figures read next.
        let debit_slot = slot(&input.debit_line, next_slot(&input.debit_line, "debit_line")?);
        let credit_slot = slot(&input.credit_line, next_slot(&input.credit_line, "credit_line")?);
        let room = open_of(&debit, true)?.min(open_of(&credit, false)?);
        let amount = match input.amount {
            Some(amount) => amount.with_scale(2)?,
            None => room,
        };
        if amount <= zero() {
            return Err(Error::msg(if room <= zero() { "there is nothing left to match on one of the lines" } else { "an amount above zero" }));
        }
        if amount > room {
            return Err(Error::msg(format!("only {room} is left to match")));
        }
        let mut data = json!({
            "debit_line": input.debit_line, "credit_line": input.credit_line, "account": account["id"], "party": party, "amount": amount,
            "date": format_date(today_date()?),
            "debit_slot": debit_slot, "credit_slot": credit_slot,
        });
        if let Some(who) = actor_name()? {
            data["by"] = json!(who);
        }
        match db::create("gl_allocation", &data) {
            Ok(row) => return Ok(row),
            // Another allocation took the same place on one of the lines: look at the figures again.
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| Error::msg("could not match the lines")))
}

fn unallocate(input: Id) -> Result<Record> {
    require_accountant()?;
    let original = require("gl_allocation", &input.id, "allocation")?;
    if dec(&original, "amount")? <= zero() {
        return Err(Error::msg("this is already an undoing"));
    }
    if db::count("gl_allocation", Filter::eq("reverses", input.id.as_str()))? > 0 {
        return Err(Error::msg("this allocation is already undone"));
    }
    let mut data = json!({
        "debit_line": original["debit_line"], "credit_line": original["credit_line"], "account": original["account"], "party": original["party"],
        "amount": zero() - dec(&original, "amount")?, "reverses": input.id, "date": format_date(today_date()?),
        "debit_slot": slot(original["debit_line"].as_str().unwrap_or_default(), next_slot(original["debit_line"].as_str().unwrap_or_default(), "debit_line")?),
        "credit_slot": slot(original["credit_line"].as_str().unwrap_or_default(), next_slot(original["credit_line"].as_str().unwrap_or_default(), "credit_line")?),
    });
    if let Some(who) = actor_name()? {
        data["by"] = json!(who);
    }
    db::create("gl_allocation", &data)
}

/// Lines on an account (and party) with something still open, and whether the search stopped at its limit.
fn open_items(input: &Items) -> Result<(Vec<Value>, bool)> {
    let account = account_by_code(&input.account)?;
    let mut find = db::find::<Record>("gl_line").filter("account", id_of(&account)?).order_by("date").limit(OPEN_PAGE);
    if let Some(party) = &input.party {
        find = find.filter("party", party.as_str());
    }
    let mut out = Vec::new();
    let lines = find.all()?;
    let truncated = lines.len() as u32 >= OPEN_PAGE;
    for line in lines {
        let debit_side = dec(&line, "debit")? > zero();
        let open = open_of(&line, debit_side)?;
        if open > zero() {
            out.push(json!({
                "line": line["id"], "entry": line["entry"], "date": line["date"], "party": line["party"],
                "side": if debit_side { "debit" } else { "credit" }, "amount": if debit_side { &line["debit"] } else { &line["credit"] }, "open": open,
            }));
        }
    }
    Ok((out, truncated))
}

/// Match what is open for a party, oldest first. Each chunk is one transaction, so a failure leaves whole chunks
/// done, never half of one; the answer says how far it got, and calling again carries on from there (the open
/// amounts are worked out afresh each time).
fn auto_allocate(input: Items) -> Result<Value> {
    require_accountant()?;
    if input.party.is_none() {
        return Err(Error::msg("name the party"));
    }
    let (items, truncated) = open_items(&input)?;
    let mut debits = Vec::new();
    let mut credits = Vec::new();
    for item in &items {
        let id = item["line"].as_str().unwrap_or_default().to_string();
        let open: Decimal = serde_json::from_value(item["open"].clone()).map_err(|e| Error::msg(format!("open: {e}")))?;
        if item["side"] == "debit" { debits.push((id, open)) } else { credits.push((id, open)) }
    }
    let pairs = fifo(&debits, &credits);
    let account = account_by_code(&input.account)?;
    let mut next: std::collections::BTreeMap<(String, bool), i64> = std::collections::BTreeMap::new();
    let mut made = 0u64;
    let mut stopped: Option<String> = None;
    for chunk in pairs.chunks(40) {
        let mut tx = db::transaction();
        let mut advanced = next.clone();
        for (debit_line, credit_line, amount) in chunk {
            let mut place = |line: &String, debit_side: bool, link: &str| -> Result<String> {
                let n = match advanced.get(&(line.clone(), debit_side)) {
                    Some(n) => *n,
                    None => next_slot(line, link)?,
                };
                advanced.insert((line.clone(), debit_side), n + 1);
                Ok(slot(line, n))
            };
            let (debit_slot, credit_slot) = (place(debit_line, true, "debit_line")?, place(credit_line, false, "credit_line")?);
            tx = tx.create(
                "gl_allocation",
                &json!({
                    "debit_line": debit_line, "credit_line": credit_line, "account": account["id"], "party": input.party,
                    "amount": amount, "date": format_date(today_date()?), "debit_slot": debit_slot, "credit_slot": credit_slot,
                }),
            );
        }
        match tx.run() {
            Ok(_) => {
                made += chunk.len() as u64;
                next = advanced;
            }
            Err(error) => {
                stopped = Some(error.to_string());
                break;
            }
        }
    }
    Ok(json!({
        "allocations": made, "planned": pairs.len(), "complete": stopped.is_none() && !truncated, "stopped_because": stopped,
        "truncated": truncated,
    }))
}

/// Each line with what is still open on it and the allocations that touch it, so a plugin that keeps documents
/// (payments) can work out what is settled without reading the ledger's tables.
fn details(input: Lines) -> Result<Value> {
    require_reader()?;
    if input.lines.len() > DETAIL_PAGE {
        return Err(Error::msg(format!("ask about at most {DETAIL_PAGE} lines at a time")));
    }
    let mut codes: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    let mut out = Vec::new();
    for id in &input.lines {
        let Some(line) = db::get::<Record>("gl_line", id)? else {
            out.push(json!({ "line": id, "missing": true }));
            continue;
        };
        let debit_side = dec(&line, "debit")? > zero();
        let account_id = text(&line, "account").unwrap_or_default().to_string();
        if !codes.contains_key(&account_id) {
            let code = db::get::<Record>("gl_account", &account_id)?.and_then(|a| text(&a, "code").map(str::to_string)).unwrap_or_default();
            codes.insert(account_id.clone(), code);
        }
        let link = if debit_side { "debit_line" } else { "credit_line" };
        let rows: Vec<Record> = db::find("gl_allocation").filter(link, id.as_str()).limit(1000).all()?;
        let mut amounts = Vec::new();
        let mut allocations = Vec::new();
        for row in &rows {
            amounts.push(dec(row, "amount")?);
            allocations.push(json!({
                "id": row["id"], "debit_line": row["debit_line"], "credit_line": row["credit_line"], "amount": row["amount"], "reverses": row["reverses"],
            }));
        }
        let amount = dec(&line, if debit_side { "debit" } else { "credit" })?;
        out.push(json!({
            "line": id, "entry": line["entry"], "date": line["date"], "account": codes[&account_id], "party": line["party"],
            "side": if debit_side { "debit" } else { "credit" }, "amount": amount, "open": open_amount(amount, &amounts), "allocations": allocations,
        }));
    }
    Ok(json!({ "lines": out }))
}

/// The lines on an account of the entries posted for a document, leaving out entries that were reversed and the
/// reversals themselves. Several entries come back when the document was posted again at a new version.
fn by_source(input: BySource) -> Result<Value> {
    require_reader()?;
    let account = account_by_code(&input.account)?;
    let entries: Vec<Record> = db::find("gl_entry").filter("source_kind", input.source_kind.as_str()).filter("source_id", input.source_id.as_str()).order_by("seq").limit(100).all()?;
    let mut out = Vec::new();
    for entry in &entries {
        let entry_id = id_of(entry)?;
        if db::count("gl_entry", Filter::eq("reverses", entry_id))? > 0 {
            continue;
        }
        let mut find = db::find::<Record>("gl_line").filter("entry", entry_id).filter("account", id_of(&account)?).limit(100);
        if let Some(party) = &input.party {
            find = find.filter("party", party.as_str());
        }
        for line in find.all()? {
            out.push(json!({
                "line": line["id"], "entry": entry_id, "number": entry["number"], "seq": entry["seq"], "date": line["date"], "party": line["party"],
                "debit": line["debit"], "credit": line["credit"], "source_version": entry["source_version"],
            }));
        }
    }
    Ok(json!({ "lines": out }))
}

handler! {
    /// What is open on each line and the allocations touching it (at most 200 lines a call).
    fn line_details(input: Lines) -> Value {
        details(input)
    }

    /// The lines on one account of the entries posted for a document (by source kind and id).
    fn source_lines(input: BySource) -> Value {
        by_source(input)
    }

    fn allocate_lines(input: Allocate) -> Record {
        allocate(input)
    }

    fn undo_allocation(input: Id) -> Record {
        unallocate(input)
    }

    fn auto_allocate_party(input: Items) -> Value {
        auto_allocate(input)
    }

    /// What is still unmatched on a control account, per line.
    fn open_items_report(input: Items) -> Value {
        require_reader()?;
        let (items, truncated) = open_items(&input)?;
        Ok(json!({ "items": items, "truncated": truncated }))
    }
}
