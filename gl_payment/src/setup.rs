//! Settings, payment terms, payment methods, and the number sequences payments are numbered from.

use aether_sdk::dates::{format_date, parse_date, Datelike, Duration, NaiveDate};
use aether_sdk::db::Filter;
use aether_sdk::prelude::*;

use crate::common::{id_of, require, require_admin, require_reader, text, Record};

#[derive(Deserialize)]
struct Setup {
    receivable_account: String,
    payable_account: String,
    #[serde(default)]
    receivable_term: Option<String>,
    #[serde(default)]
    payable_term: Option<String>,
    #[serde(default)]
    seq_in: Option<String>,
    #[serde(default)]
    seq_out: Option<String>,
    #[serde(default)]
    four_eyes_out: Option<bool>,
}

#[derive(Deserialize)]
struct NewTerm {
    name: String,
    net_days: i64,
    #[serde(default)]
    end_of_month: bool,
}

#[derive(Deserialize)]
struct PartyTerm {
    party: String,
    side: String,
    term: String,
}

#[derive(Deserialize)]
struct NewMethod {
    name: String,
    #[serde(default)]
    kind: Option<String>,
    account: String,
    journal: String,
}

#[derive(Deserialize)]
struct Id {
    id: String,
}

/// The ledger account behind a code, checked: it exists, is active and is not a group.
fn ledger_account(code: &str) -> Result<Value> {
    let accounts: Vec<Record> = crate::common::call("gl", "list_accounts", &json!({}))?;
    let account = accounts.into_iter().find(|a| text(a, "code") == Some(code)).ok_or_else(|| Error::msg(format!("the ledger has no account `{code}`")))?;
    if account.get("is_active") == Some(&json!(false)) || account.get("is_group") == Some(&json!(true)) {
        return Err(Error::msg(format!("account `{code}` cannot be posted to (inactive or a group)")));
    }
    Ok(account)
}

/// A gapless sequence numbered per year, made when it is missing.
fn ensure_sequence(code: &str, label: &str, prefix: &str) -> Result<()> {
    if crate::common::call::<Value>("number_sequence", "get_sequence", &json!({ "code": code })).is_ok() {
        return Ok(());
    }
    crate::common::call::<Value>(
        "number_sequence",
        "create_sequence",
        &json!({ "code": code, "label": label, "prefix": prefix, "padding": 5, "reset": "year", "gapless": true }),
    )?;
    Ok(())
}

fn setup(input: Setup) -> Result<Record> {
    require_admin()?;
    for code in [&input.receivable_account, &input.payable_account] {
        let account = ledger_account(code)?;
        if account.get("is_control") != Some(&json!(true)) {
            return Err(Error::msg(format!("account `{code}` must be a control account, so every line on it names its party")));
        }
    }
    for term in [&input.receivable_term, &input.payable_term].into_iter().flatten() {
        require("pay_term", term, "payment term")?;
    }
    let seq_in = input.seq_in.unwrap_or_else(|| "pay_in".into());
    let seq_out = input.seq_out.unwrap_or_else(|| "pay_out".into());
    ensure_sequence(&seq_in, "Incoming payments", "PAY-IN-{YYYY}-")?;
    ensure_sequence(&seq_out, "Outgoing payments", "PAY-OUT-{YYYY}-")?;
    let data = json!({
        "receivable_account": input.receivable_account, "payable_account": input.payable_account,
        "receivable_term": input.receivable_term, "payable_term": input.payable_term,
        "seq_in": seq_in, "seq_out": seq_out, "four_eyes_out": input.four_eyes_out.unwrap_or(true),
    });
    match db::find::<Record>("pay_setting").first()? {
        Some(existing) => db::update::<Record>("pay_setting", id_of(&existing)?, &data)?.ok_or_else(|| Error::msg("the settings are gone")),
        None => db::create("pay_setting", &data),
    }
}

fn new_term(input: NewTerm) -> Result<Record> {
    require_admin()?;
    if input.name.trim().is_empty() {
        return Err(Error::msg("name the payment term"));
    }
    if !(0..=3650).contains(&input.net_days) {
        return Err(Error::msg("net days are 0 to 3650"));
    }
    db::create("pay_term", &json!({ "name": input.name.trim(), "net_days": input.net_days, "end_of_month": input.end_of_month, "is_active": true }))
        .map_err(|e| e.or("could not create the term (the name may be taken)"))
}

fn party_term(input: PartyTerm) -> Result<Record> {
    require_admin()?;
    if !matches!(input.side.as_str(), "receivable" | "payable") {
        return Err(Error::msg("the side is receivable or payable"));
    }
    require("pay_term", &input.term, "payment term")?;
    let key = format!("{}|{}", input.party, input.side);
    match db::find::<Record>("pay_party_term").filter("key", key.as_str()).first()? {
        Some(existing) => db::update::<Record>("pay_party_term", id_of(&existing)?, &json!({ "term": input.term }))?.ok_or_else(|| Error::msg("the party term is gone")),
        None => db::create("pay_party_term", &json!({ "party": input.party, "side": input.side, "term": input.term, "key": key })),
    }
}

fn new_method(input: NewMethod) -> Result<Record> {
    require_admin()?;
    let kind = input.kind.unwrap_or_else(|| "bank".into());
    if !matches!(kind.as_str(), "bank" | "cash") {
        return Err(Error::msg("a method is bank or cash"));
    }
    let account = ledger_account(&input.account)?;
    if account.get("is_control") == Some(&json!(true)) {
        return Err(Error::msg("money moves through a bank or cash account, not a control account"));
    }
    db::create("pay_method", &json!({ "name": input.name.trim(), "kind": kind, "account": input.account, "journal": input.journal, "is_active": true }))
        .map_err(|e| e.or("could not create the method (the name may be taken)"))
}

/// The last day of the month a date is in.
fn month_end(date: NaiveDate) -> NaiveDate {
    let (year, month) = if date.month() == 12 { (date.year() + 1, 1) } else { (date.year(), date.month() + 1) };
    NaiveDate::from_ymd_opt(year, month, 1).map(|first| first - Duration::days(1)).unwrap_or(date)
}

/// When a document is due: its date plus the party's term for that side, else the default term, else the date.
pub fn due_date(settings: &Record, party: &str, side: &str, date: &str) -> Result<String> {
    let date = parse_date(date.get(..10).unwrap_or(date))?;
    let key = format!("{party}|{side}");
    let term_id = match db::find::<Record>("pay_party_term").filter("key", key.as_str()).first()? {
        Some(row) => text(&row, "term").map(str::to_string),
        None => text(settings, if side == "receivable" { "receivable_term" } else { "payable_term" }).map(str::to_string),
    };
    let Some(term_id) = term_id else {
        return Ok(format_date(date));
    };
    let term = require("pay_term", &term_id, "payment term")?;
    let mut due = date + Duration::days(term.get("net_days").and_then(Value::as_i64).unwrap_or(0));
    if term.get("end_of_month") == Some(&json!(true)) {
        due = month_end(due);
    }
    Ok(format_date(due))
}

/// Settings for an organization that has none, against the accounts and journals the `gl` seed
/// creates (1100 receivable, 2100 payable, 1010 cash, 1020 bank): the two payment sequences, two
/// terms and a bank and a cash method. Each only when missing; nothing that exists is changed.
/// The ledger's accounts are not checked here (the kernel holds no ledger role), so an
/// organization that built its own chart sets payments up by hand instead.
fn seed() -> Result<Value> {
    ensure_sequence("pay_in", "Incoming payments", "PAY-IN-{YYYY}-")?;
    ensure_sequence("pay_out", "Outgoing payments", "PAY-OUT-{YYYY}-")?;
    let mut terms = 0;
    for (name, days) in [("Due on receipt", 0), ("Net 30", 30)] {
        if db::find::<Record>("pay_term").filter("name", name).first()?.is_none() {
            db::create::<Record>("pay_term", &json!({ "name": name, "net_days": days, "end_of_month": false, "is_active": true }))?;
            terms += 1;
        }
    }
    let mut methods = 0;
    for (name, kind, account, journal) in [("Bank transfer", "bank", "1020", "BNK"), ("Cash", "cash", "1010", "CSH")] {
        if db::find::<Record>("pay_method").filter("name", name).first()?.is_none() {
            db::create::<Record>("pay_method", &json!({ "name": name, "kind": kind, "account": account, "journal": journal, "is_active": true }))?;
            methods += 1;
        }
    }
    let mut settings = false;
    if db::find::<Record>("pay_setting").first()?.is_none() {
        db::create::<Record>(
            "pay_setting",
            &json!({ "receivable_account": "1100", "payable_account": "2100", "seq_in": "pay_in", "seq_out": "pay_out", "four_eyes_out": true }),
        )?;
        settings = true;
    }
    Ok(json!({ "settings": settings, "terms": terms, "methods": methods }))
}

handler! {
    /// Set the receivable and payable control accounts, default terms and numbering; run again to change them.
    /// Default payment settings, terms and methods when there are none; runs on install and upgrade.
    fn seed_payments(_: Empty) -> Value {
        seed()
    }

    fn setup_payments(input: Setup) -> Record {
        setup(input)
    }

    fn get_settings(_: Empty) -> Option<Record> {
        require_reader()?;
        db::find("pay_setting").first()
    }

    fn create_term(input: NewTerm) -> Record {
        new_term(input)
    }

    fn list_terms(_: Empty) -> Vec<Record> {
        require_reader()?;
        db::find("pay_term").order_by("name").limit(500).all()
    }

    /// The term a party gets on one side (what they owe us, or what we owe them).
    fn set_party_term(input: PartyTerm) -> Record {
        party_term(input)
    }

    fn create_method(input: NewMethod) -> Record {
        new_method(input)
    }

    fn deactivate_method(input: Id) -> Record {
        require_admin()?;
        require("pay_method", &input.id, "payment method")?;
        db::update::<Record>("pay_method", &input.id, &json!({ "is_active": false }))?.ok_or_else(|| Error::msg("the method is gone"))
    }

    fn list_methods(_: Empty) -> Vec<Record> {
        require_reader()?;
        db::find("pay_method").matching(Filter::ne("is_active", false)).order_by("name").limit(500).all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn month_end_handles_december_and_leap_years() {
        let d = |s: &str| parse_date(s).unwrap();
        assert_eq!(month_end(d("2026-12-05")), d("2026-12-31"));
        assert_eq!(month_end(d("2028-02-10")), d("2028-02-29"));
        assert_eq!(month_end(d("2026-04-30")), d("2026-04-30"));
    }
}
