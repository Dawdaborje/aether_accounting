//! Payments: drafted by a clerk, confirmed by an accountant.
//!
//! Confirming numbers the payment, posts it to the ledger and matches it against the documents it pays. Each step
//! can be repeated: the number is kept on the payment once drawn, the ledger returns the same entry for the same
//! payment, and a match only adds what is still missing between two lines. So a confirm cut off halfway is
//! finished by confirming again, and never posts or matches twice.

use aether_sdk::decimal::Decimal;
use aether_sdk::prelude::*;

use crate::common::{
    actor_name, control_account, dec, dec_value, id_of, ledger_currency, line_details, matched_between, money, require, require_accountant, require_clerk,
    require_reader, settings, side_of, text, zero, Record, PAYMENT_SOURCE,
};
use crate::documents::refresh;

#[derive(Deserialize)]
struct Apply {
    document: String,
    amount: Decimal,
}

#[derive(Deserialize)]
struct NewPayment {
    direction: String,
    party: String,
    method: String,
    date: String,
    amount: Decimal,
    #[serde(default)]
    company: Option<String>,
    #[serde(default)]
    reference: Option<String>,
    #[serde(default)]
    memo: Option<String>,
    #[serde(default)]
    applies: Vec<Apply>,
}

#[derive(Deserialize)]
struct DraftChange {
    id: String,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    date: Option<String>,
    #[serde(default)]
    amount: Option<Decimal>,
    #[serde(default)]
    reference: Option<String>,
    #[serde(default)]
    memo: Option<String>,
}

#[derive(Deserialize)]
struct ApplyTo {
    payment: String,
    document: String,
    amount: Decimal,
}

#[derive(Deserialize)]
struct Cancel {
    id: String,
    reason: String,
}

#[derive(Deserialize)]
struct Advance {
    payment: String,
    document: String,
    #[serde(default)]
    amount: Option<Decimal>,
}

#[derive(Deserialize)]
struct Id {
    id: String,
}

fn draft(id: &str) -> Result<Record> {
    let payment = require("payment", id, "payment")?;
    if text(&payment, "state") != Some("draft") {
        return Err(Error::msg("only a draft payment can be changed"));
    }
    Ok(payment)
}

fn active_method(id: &str) -> Result<Record> {
    let method = require("pay_method", id, "payment method")?;
    if method.get("is_active") == Some(&json!(false)) {
        return Err(Error::msg("this payment method is not in use any more"));
    }
    Ok(method)
}

/// A document a payment may settle: same party, the side the payment's direction settles, not void.
fn payable_document(payment: &Record, document_id: &str) -> Result<Record> {
    let document = require("pay_document", document_id, "document")?;
    let side = side_of(text(payment, "direction").unwrap_or_default())?;
    if text(&document, "side") != Some(side) {
        return Err(Error::msg(format!("document {} is not {side}", text(&document, "number").unwrap_or("?"))));
    }
    if text(&document, "party") != text(payment, "party") {
        return Err(Error::msg(format!("document {} is for another party", text(&document, "number").unwrap_or("?"))));
    }
    if text(&document, "state") == Some("void") {
        return Err(Error::msg(format!("document {} is void", text(&document, "number").unwrap_or("?"))));
    }
    Ok(document)
}

/// Set what a draft payment settles on one document; zero takes it off. One row per payment and document.
fn set_apply(payment: &Record, document_id: &str, amount: Decimal) -> Result<Record> {
    let amount = money(amount)?;
    if amount < zero() {
        return Err(Error::msg("an amount of zero or more"));
    }
    payable_document(payment, document_id)?;
    let key = format!("{}|{}", id_of(payment)?, document_id);
    match db::find::<Record>("payment_apply").filter("key", key.as_str()).first()? {
        Some(row) => db::update::<Record>("payment_apply", id_of(&row)?, &json!({ "amount": amount }))?.ok_or_else(|| Error::msg("the row is gone")),
        None => db::create("payment_apply", &json!({ "payment": payment["id"], "document": document_id, "amount": amount, "key": key })),
    }
}

fn applies_of(payment_id: &str) -> Result<Vec<Record>> {
    db::find("payment_apply").filter("payment", payment_id).limit(500).all()
}

fn create(input: NewPayment) -> Result<Value> {
    require_clerk()?;
    side_of(&input.direction)?;
    if input.party.trim().is_empty() {
        return Err(Error::msg("name the party paid or paying"));
    }
    let amount = money(input.amount)?;
    if amount <= zero() {
        return Err(Error::msg("a payment's amount is above zero"));
    }
    active_method(&input.method)?;
    // Every document is checked before anything is written, so a refused payment leaves no draft behind.
    let draft_like = json!({ "direction": input.direction, "party": input.party.trim() });
    for apply in &input.applies {
        payable_document(&draft_like, &apply.document)?;
        if money(apply.amount)? < zero() {
            return Err(Error::msg("an amount of zero or more"));
        }
    }
    let payment: Record = db::create(
        "payment",
        &json!({
            "direction": input.direction, "party": input.party.trim(), "method": input.method, "date": input.date.get(..10).unwrap_or(&input.date),
            "amount": amount, "company": input.company, "reference": input.reference, "memo": input.memo, "state": "draft", "created_by": actor_name()?,
        }),
    )?;
    for apply in input.applies {
        set_apply(&payment, &apply.document, apply.amount)?;
    }
    full(id_of(&payment)?)
}

fn change(input: DraftChange) -> Result<Value> {
    require_clerk()?;
    draft(&input.id)?;
    let mut data = json!({});
    if let Some(method) = input.method {
        active_method(&method)?;
        data["method"] = json!(method);
    }
    if let Some(date) = input.date {
        data["date"] = json!(date.get(..10).unwrap_or(&date));
    }
    if let Some(amount) = input.amount {
        let amount = money(amount)?;
        if amount <= zero() {
            return Err(Error::msg("a payment's amount is above zero"));
        }
        data["amount"] = json!(amount);
    }
    if let Some(reference) = input.reference {
        data["reference"] = json!(reference);
    }
    if let Some(memo) = input.memo {
        data["memo"] = json!(memo);
    }
    db::update::<Record>("payment", &input.id, &data)?;
    full(&input.id)
}

/// The payment with what it settles.
fn full(id: &str) -> Result<Value> {
    let payment = require("payment", id, "payment")?;
    let applies = applies_of(id)?;
    Ok(json!({ "payment": payment, "applies": applies }))
}

/// Allocate between a payment's line and a document's line until `target` is matched between them, capped at what
/// is open on both. Returns the allocation written, if one was needed.
fn match_up(direction: &str, payment_line: &str, document_line: &str, target: Decimal) -> Result<Option<String>> {
    let details = line_details(&[payment_line.to_string(), document_line.to_string()])?;
    let payment_detail = details.get(payment_line).ok_or_else(|| Error::msg("the payment's ledger line is missing"))?;
    let document_detail = details.get(document_line).ok_or_else(|| Error::msg("the document's ledger line is missing"))?;
    // Money in credits the receivable account against the invoice's debit; money out the other way round.
    let (debit_line, credit_line) = if direction == "in" { (document_line, payment_line) } else { (payment_line, document_line) };
    let done = matched_between(payment_detail, debit_line, credit_line)?;
    let missing = target - done;
    if missing <= zero() {
        return Ok(None);
    }
    let room = dec_value(&payment_detail["open"], "open")?.min(dec_value(&document_detail["open"], "open")?);
    let amount = missing.min(room);
    if amount <= zero() {
        return Ok(None);
    }
    let row: Record = crate::common::call("gl", "allocate_lines", &json!({ "debit_line": debit_line, "credit_line": credit_line, "amount": amount }))?;
    Ok(text(&row, "id").map(str::to_string))
}

fn confirm(input: Id) -> Result<Value> {
    require_accountant()?;
    let payment = require("payment", &input.id, "payment")?;
    match text(&payment, "state") {
        Some("confirmed") => return full(&input.id),
        Some("draft") => {}
        _ => return Err(Error::msg("a cancelled payment cannot be confirmed")),
    }
    let settings = settings()?;
    let direction = text(&payment, "direction").unwrap_or_default().to_string();
    let me = actor_name()?;
    if direction == "out" && settings.get("four_eyes_out") != Some(&json!(false)) && me.is_some() && me.as_deref() == text(&payment, "created_by") {
        return Err(Error::msg("money going out is confirmed by someone other than the person who drafted the payment"));
    }
    let method = active_method(text(&payment, "method").unwrap_or_default())?;
    let amount = dec(&payment, "amount")?;
    let side = side_of(&direction)?;
    let control = control_account(&settings, side)?;
    let party = text(&payment, "party").unwrap_or_default().to_string();

    // What it settles is checked before anything is written.
    let applies = applies_of(&input.id)?;
    let mut planned = Vec::new();
    let mut applied_total = zero();
    for apply in &applies {
        let target = dec(apply, "amount")?;
        if target.is_zero() {
            continue;
        }
        let document = payable_document(&payment, text(apply, "document").unwrap_or_default())?;
        let line = text(&document, "gl_line")
            .ok_or_else(|| Error::msg(format!("document {} is not in the ledger yet: try again once it is posted", text(&document, "number").unwrap_or("?"))))?
            .to_string();
        applied_total = applied_total + target;
        planned.push((apply.clone(), document, line, target));
    }
    if applied_total > amount {
        return Err(Error::msg(format!("the payment is {amount} but {applied_total} is applied to documents")));
    }
    // No more than is open on each document, counting what this payment already matched on an earlier try.
    let earlier_line = text(&payment, "gl_line").map(str::to_string);
    let mut asked: Vec<String> = planned.iter().map(|(_, _, line, _)| line.clone()).collect();
    asked.extend(earlier_line.clone());
    let details = line_details(&asked)?;
    for (_, document, line, target) in &planned {
        let detail = details.get(line).ok_or_else(|| Error::msg("a document's ledger line is missing"))?;
        let done = match &earlier_line {
            Some(own) => {
                let (debit_line, credit_line) = if direction == "in" { (line.as_str(), own.as_str()) } else { (own.as_str(), line.as_str()) };
                details.get(own).map(|d| matched_between(d, debit_line, credit_line)).transpose()?.unwrap_or_else(zero)
            }
            None => zero(),
        };
        let open = dec_value(&detail["open"], "open")?;
        if *target - done > open {
            return Err(Error::msg(format!("document {} has only {open} open", text(document, "number").unwrap_or("?"))));
        }
    }
    let date = text(&payment, "date").unwrap_or_default().to_string();
    let base = ledger_currency()?;

    // 1. The number, drawn once and kept.
    let number = match text(&payment, "number") {
        Some(number) => number.to_string(),
        None => {
            let code = text(&settings, if direction == "in" { "seq_in" } else { "seq_out" }).unwrap_or(if direction == "in" { "pay_in" } else { "pay_out" });
            let drawn: Value =
                crate::common::call("number_sequence", "next_number", &json!({ "code": code, "date": date, "scope": text(&payment, "company"), "reference": input.id }))?;
            let number = drawn["number"].as_str().ok_or_else(|| Error::msg("the number sequence gave no number"))?.to_string();
            db::update::<Record>("payment", &input.id, &json!({ "number": number }))?;
            number
        }
    };

    // 2. The entry: the ledger returns the one already posted for this payment.
    let (debit, credit) = if direction == "in" { (text(&method, "account").unwrap_or_default(), control.as_str()) } else { (control.as_str(), text(&method, "account").unwrap_or_default()) };
    let party_of = |account: &str| if account == control { Some(party.clone()) } else { None };
    let entry: Record = crate::common::call(
        "gl",
        "post_sourced_entry",
        &json!({
            "journal": text(&method, "journal"), "date": date, "currency": base,
            "memo": format!("Payment {number}{}", text(&payment, "reference").map(|r| format!(" ({r})")).unwrap_or_default()),
            "lines": [
                { "account": debit, "debit": amount, "party": party_of(debit) },
                { "account": credit, "credit": amount, "party": party_of(credit) },
            ],
            "source": { "kind": PAYMENT_SOURCE, "id": input.id },
        }),
    )?;
    let lines: Value = crate::common::call(
        "gl",
        "source_lines",
        &json!({ "source_kind": format!("doc:{PAYMENT_SOURCE}"), "source_id": input.id, "account": control, "party": party }),
    )?;
    let payment_line = lines["lines"]
        .as_array()
        .and_then(|l| l.iter().find(|l| l["entry"] == entry["id"]))
        .and_then(|l| l["line"].as_str())
        .ok_or_else(|| Error::msg(format!("entry {} is posted, but its lines are not indexed yet: confirm again in a few minutes", text(&entry, "number").unwrap_or("?"))))?
        .to_string();
    db::update::<Record>("payment", &input.id, &json!({ "gl_entry": entry["id"], "gl_line": payment_line }))?;

    // 3. The matches, each only for what is still missing.
    let mut documents = Vec::new();
    for (apply, document, line, target) in planned {
        if let Some(allocation) = match_up(&direction, &payment_line, &line, target)? {
            db::update::<Record>("payment_apply", id_of(&apply)?, &json!({ "allocation": allocation }))?;
        }
        documents.push(document);
    }

    db::update::<Record>("payment", &input.id, &json!({ "state": "confirmed", "confirmed_by": me }))?;
    refresh(&documents)?;
    events::emit(
        "payment_confirmed",
        &json!({ "payment": input.id, "number": number, "direction": direction, "party": party, "amount": amount, "date": date, "entry": entry["number"] }),
    )?;
    full(&input.id)
}

fn cancel(input: Cancel) -> Result<Value> {
    require_accountant()?;
    if input.reason.trim().is_empty() {
        return Err(Error::msg("say why the payment is cancelled"));
    }
    let payment = require("payment", &input.id, "payment")?;
    let settings = settings()?;
    let direction = text(&payment, "direction").unwrap_or_default().to_string();
    match text(&payment, "state") {
        Some("cancelled") => return full(&input.id),
        Some("draft") => {
            // A number drawn by a confirm that did not finish is written off, so the gap audit explains it.
            if let (Some(number), None) = (text(&payment, "number"), text(&payment, "gl_entry")) {
                let code = text(&settings, if direction == "in" { "seq_in" } else { "seq_out" }).unwrap_or_default();
                crate::common::call::<Value>("number_sequence", "void_number", &json!({ "code": code, "scope": text(&payment, "company"), "number": number, "reason": input.reason }))?;
            }
        }
        _ => {}
    }
    // Posted (a confirmed payment, or a draft whose confirm stopped after posting): undo its matches, then reverse.
    let mut documents = Vec::new();
    if let (Some(entry), Some(line)) = (text(&payment, "gl_entry"), text(&payment, "gl_line")) {
        let details = line_details(&[line.to_string()])?;
        let detail = details.get(line).ok_or_else(|| Error::msg("the payment's ledger line is missing"))?;
        let rows = detail["allocations"].as_array().cloned().unwrap_or_default();
        let undone: Vec<&str> = rows.iter().filter_map(|r| r["reverses"].as_str()).collect();
        for row in &rows {
            let id = row["id"].as_str().unwrap_or_default();
            if dec_value(&row["amount"], "amount")? > zero() && !undone.contains(&id) {
                crate::common::call::<Value>("gl", "undo_allocation", &json!({ "id": id }))?;
            }
        }
        let reversal: Record = crate::common::call("gl", "reverse_entry", &json!({ "id": entry, "reason": input.reason }))
            .or_else(|error| {
                // Reversed by an earlier cancel that stopped before it was recorded here.
                if error.to_string().contains("already reversed") {
                    Ok(json!({}))
                } else {
                    Err(error)
                }
            })?;
        db::update::<Record>("payment", &input.id, &json!({ "reverse_entry": reversal.get("id") }))?;
        for apply in applies_of(&input.id)? {
            documents.push(require("pay_document", text(&apply, "document").unwrap_or_default(), "document")?);
        }
    }
    db::update::<Record>("payment", &input.id, &json!({ "state": "cancelled", "cancelled_by": actor_name()?, "cancel_reason": input.reason }))?;
    refresh(&documents)?;
    events::emit("payment_cancelled", &json!({ "payment": input.id, "number": payment["number"], "direction": direction, "party": payment["party"], "reason": input.reason }))?;
    full(&input.id)
}

/// Match what is left on a confirmed payment (an advance or an over-payment) against a document.
fn advance(input: Advance) -> Result<Value> {
    require_accountant()?;
    let payment = require("payment", &input.payment, "payment")?;
    if text(&payment, "state") != Some("confirmed") {
        return Err(Error::msg("only a confirmed payment has money to apply"));
    }
    let document = payable_document(&payment, &input.document)?;
    let document_line = text(&document, "gl_line").ok_or_else(|| Error::msg("the document is not in the ledger yet"))?.to_string();
    let payment_line = text(&payment, "gl_line").ok_or_else(|| Error::msg("the payment has no ledger line"))?.to_string();
    let details = line_details(&[payment_line.clone(), document_line.clone()])?;
    let room = dec_value(&details[&payment_line]["open"], "open")?.min(dec_value(&details[&document_line]["open"], "open")?);
    let amount = match input.amount {
        Some(amount) => money(amount)?,
        None => room,
    };
    if amount <= zero() || amount > room {
        return Err(Error::msg(format!("{room} can be applied")));
    }
    let direction = text(&payment, "direction").unwrap_or_default().to_string();
    let key = format!("{}|{}", input.payment, input.document);
    let existing = db::find::<Record>("payment_apply").filter("key", key.as_str()).first()?;
    let before = match &existing {
        Some(row) => dec(row, "amount")?,
        None => zero(),
    };
    // The target between the two lines grows by the amount; what is matched already stays.
    let (debit_line, credit_line) = if direction == "in" { (&document_line, &payment_line) } else { (&payment_line, &document_line) };
    let target = matched_between(&details[&payment_line], debit_line, credit_line)? + amount;
    let allocation = match_up(&direction, &payment_line, &document_line, target)?;
    let row = json!({ "amount": before + amount, "allocation": allocation });
    match existing {
        Some(found) => {
            db::update::<Record>("payment_apply", id_of(&found)?, &row)?;
        }
        None => {
            db::create::<Record>("payment_apply", &json!({ "payment": input.payment, "document": input.document, "amount": amount, "key": key, "allocation": allocation }))?;
        }
    }
    refresh(&[document])?;
    full(&input.payment)
}

handler! {
    /// Draft a payment in or out, optionally with the documents it settles (`applies`).
    fn create_payment(input: NewPayment) -> Value {
        create(input)
    }

    fn update_draft(input: DraftChange) -> Value {
        change(input)
    }

    /// Set how much of a draft payment goes to one document (zero removes it).
    fn apply_to(input: ApplyTo) -> Value {
        require_clerk()?;
        let payment = draft(&input.payment)?;
        set_apply(&payment, &input.document, input.amount)?;
        full(&input.payment)
    }

    /// Number, post and match the payment. Safe to call again after a failure.
    fn confirm_payment(input: Id) -> Value {
        confirm(input)
    }

    /// Undo the payment's matches and reverse its entry, with a reason.
    fn cancel_payment(input: Cancel) -> Value {
        cancel(input)
    }

    /// Apply what is left of a confirmed payment to another document.
    fn apply_advance(input: Advance) -> Value {
        advance(input)
    }

    fn get_payment(input: Id) -> Value {
        require_reader()?;
        full(&input.id)
    }
}
