//! Documents: an invoice or bill the ledger holds open on a control account, as the bridges announce it.
//!
//! This plugin keeps who, which number and when it is due; what is still open always comes from the ledger (the
//! document's control line minus its allocations). `state` is only a cache of that, refreshed after each match and
//! by the schedule.

use aether_sdk::db::Filter;
use aether_sdk::decimal::Decimal;
use aether_sdk::prelude::*;

use crate::common::{
    control_account, dec, dec_value, id_of, ledger_currency, line_details, money, require, require_kernel_or_admin, settings, text, zero, Record,
};
use crate::setup::due_date;

#[derive(Deserialize)]
pub struct Register {
    side: String,
    source_kind: String,
    source_id: String,
    #[serde(default)]
    version: Option<i64>,
    #[serde(default)]
    number: Option<String>,
    party: String,
    #[serde(default)]
    company: Option<String>,
    #[serde(default)]
    currency: Option<String>,
    date: String,
    total: Decimal,
    #[serde(default)]
    due_date: Option<String>,
}

#[derive(Deserialize)]
struct Source {
    source_kind: String,
    source_id: String,
}

/// Documents looked at by one tick of the schedule.
const TICK: u32 = 200;

fn register(input: Register) -> Result<Record> {
    require_kernel_or_admin()?;
    if !matches!(input.side.as_str(), "receivable" | "payable") {
        return Err(Error::msg("the side is receivable or payable"));
    }
    if input.party.trim().is_empty() || input.source_kind.trim().is_empty() || input.source_id.trim().is_empty() {
        return Err(Error::msg("a document names its source and its party"));
    }
    let total = money(input.total)?;
    if total <= zero() {
        return Err(Error::msg("a document's total is above zero"));
    }
    let base = ledger_currency()?;
    if let Some(currency) = input.currency.as_deref().filter(|c| !c.is_empty()) {
        if currency != base {
            return Err(Error::msg(format!("the ledger keeps {base}: documents in {currency} are not supported yet")));
        }
    }
    let settings = settings()?;
    let version = input.version.unwrap_or(1);
    let key = format!("{}:{}", input.source_kind, input.source_id);
    let due = match &input.due_date {
        Some(due) if !due.is_empty() => due.get(..10).unwrap_or(due).to_string(),
        _ => due_date(&settings, &input.party, &input.side, &input.date)?,
    };
    let mut data = json!({
        "side": input.side, "source_kind": input.source_kind, "source_id": input.source_id, "key": key, "version": version,
        "number": input.number, "party": input.party, "company": input.company, "currency": base,
        "date": input.date.get(..10).unwrap_or(&input.date), "due_date": due, "total": total,
    });
    let document = match db::find::<Record>("pay_document").filter("key", key.as_str()).first()? {
        // Delivered again: nothing changes.
        Some(existing) if existing.get("version").and_then(Value::as_i64).unwrap_or(1) >= version => existing,
        // A new version of the document was approved: the ledger posts it again, so the line is found again.
        Some(existing) => {
            data["gl_line"] = Value::Null;
            data["gl_entry_number"] = Value::Null;
            data["state"] = json!("open");
            db::update::<Record>("pay_document", id_of(&existing)?, &data)?.ok_or_else(|| Error::msg("the document is gone"))?
        }
        None => {
            data["state"] = json!("open");
            match db::create::<Record>("pay_document", &data) {
                Ok(row) => row,
                // The same delivery twice at once: the other one made it.
                Err(error) => db::find::<Record>("pay_document").filter("key", key.as_str()).first()?.ok_or(error)?,
            }
        }
    };
    // The ledger may not have posted it yet (events are delivered one by one); the schedule tries again later.
    Ok(link(&settings, &document)?.unwrap_or(document))
}

/// Find the document's line on its control account and remember it. None when the ledger has not posted it yet.
pub fn link(settings: &Record, document: &Record) -> Result<Option<Record>> {
    if text(document, "gl_line").is_some() {
        return Ok(None);
    }
    let side = text(document, "side").unwrap_or_default();
    let answer: Value = crate::common::call(
        "gl",
        "source_lines",
        &json!({
            "source_kind": text(document, "source_kind"), "source_id": text(document, "source_id"),
            "account": control_account(settings, side)?, "party": text(document, "party"),
        }),
    )?;
    let version = document.get("version").and_then(Value::as_i64).unwrap_or(1);
    let lines = answer["lines"].as_array().cloned().unwrap_or_default();
    // The entry of this version of the document; a rule without versions posts none, so then the latest entry.
    let wanted = if side == "receivable" { "debit" } else { "credit" };
    let pick = lines
        .iter()
        .filter(|l| !dec_value(&l[wanted], wanted).unwrap_or_else(|_| zero()).is_zero())
        .filter(|l| l["source_version"].as_i64().is_none_or(|v| v == version))
        .max_by_key(|l| l["seq"].as_i64().unwrap_or(0));
    let Some(line) = pick else {
        return Ok(None);
    };
    let updated = db::update::<Record>("pay_document", id_of(document)?, &json!({ "gl_line": line["line"], "gl_entry_number": line["number"] }))?;
    Ok(updated)
}

/// Bring the cached state of documents in line with the ledger; announce the ones that became settled.
pub fn refresh(documents: &[Record]) -> Result<u64> {
    let linked: Vec<&Record> = documents.iter().filter(|d| text(d, "gl_line").is_some() && text(d, "state") != Some("void")).collect();
    let ids: Vec<String> = linked.iter().filter_map(|d| text(d, "gl_line").map(str::to_string)).collect();
    let details = line_details(&ids)?;
    let mut settled = 0;
    for document in linked {
        let Some(detail) = text(document, "gl_line").and_then(|l| details.get(l)) else {
            continue;
        };
        let open = dec_value(&detail["open"], "open")?;
        let state = if open.is_zero() { "settled" } else { "open" };
        if text(document, "state") == Some(state) {
            continue;
        }
        db::update::<Record>("pay_document", id_of(document)?, &json!({ "state": state }))?;
        if state == "settled" {
            settled += 1;
            events::emit(
                "document_settled",
                &json!({
                    "document": document["id"], "side": document["side"], "source_kind": document["source_kind"], "source_id": document["source_id"],
                    "number": document["number"], "party": document["party"], "total": document["total"],
                }),
            )?;
        }
    }
    Ok(settled)
}

/// Open documents with what is still open on each, worked out by the ledger.
pub fn open_documents(filter: Filter) -> Result<(Vec<(Record, Decimal)>, bool)> {
    let rows: Vec<Record> = db::find("pay_document").matching(filter.and(Filter::eq("state", "open"))).order_by("due_date").limit(1000).all()?;
    let truncated = rows.len() >= 1000;
    let ids: Vec<String> = rows.iter().filter_map(|d| text(d, "gl_line").map(str::to_string)).collect();
    let details = line_details(&ids)?;
    let mut out = Vec::new();
    for document in rows {
        let open = match text(&document, "gl_line").and_then(|l| details.get(l)) {
            Some(detail) => dec_value(&detail["open"], "open")?,
            // Not posted by the ledger yet: nothing can be paid against it, but it is still owed.
            None => dec(&document, "total")?,
        };
        if open > zero() {
            out.push((document, open));
        }
    }
    Ok((out, truncated))
}

handler! {
    /// Called by the bridge plugins when an invoice or a bill is approved (or by an administrator for an old one).
    /// Delivering the same document again changes nothing; a higher version replaces it.
    fn register_document(input: Register) -> Record {
        register(input)
    }

    /// The document was cancelled at its source. Refused while payments are still matched against it.
    fn void_document(input: Source) -> Record {
        require_kernel_or_admin()?;
        let key = format!("{}:{}", input.source_kind, input.source_id);
        let document = db::find::<Record>("pay_document").filter("key", key.as_str()).first()?.ok_or_else(|| Error::msg("there is no such document"))?;
        let applied = db::find::<Record>("payment_apply").filter("document", id_of(&document)?).limit(500).all()?;
        for apply in &applied {
            let payment = require("payment", text(apply, "payment").unwrap_or_default(), "payment")?;
            if text(&payment, "state") == Some("confirmed") && dec(apply, "amount")? > zero() {
                return Err(Error::msg(format!("payment {} is matched against it: cancel that payment first", text(&payment, "number").unwrap_or("?"))));
            }
        }
        db::update::<Record>("pay_document", id_of(&document)?, &json!({ "state": "void" }))?.ok_or_else(|| Error::msg("the document is gone"))
    }

    /// Every 10 minutes: find the ledger lines of documents posted since they were registered, and refresh the
    /// state of open documents.
    fn link_documents(_: Empty) -> Value {
        let settings = settings()?;
        let unlinked: Vec<Record> = db::find("pay_document").matching(Filter::eq("state", "open").and(Filter::is_empty("gl_line"))).limit(TICK).all()?;
        let mut linked = 0u64;
        for document in &unlinked {
            if link(&settings, document)?.is_some() {
                linked += 1;
            }
        }
        let open: Vec<Record> = db::find("pay_document").matching(Filter::eq("state", "open").and(Filter::is_set("gl_line"))).order_by("due_date").limit(TICK).all()?;
        let settled = refresh(&open)?;
        Ok(json!({ "linked": linked, "still_unlinked": unlinked.len() as u64 - linked, "settled": settled }))
    }
}
