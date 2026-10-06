//! The ledger itself, accounts, journals, periods and posting rules.

use aether_sdk::dates::{format_date, NaiveDate};
use aether_sdk::db::Filter;
use aether_sdk::prelude::*;

use crate::common::{account_by_code, id_of, journal_by_code, ledger, pick, require_admin, text, Record};
use crate::rules::apply_template;

const KINDS: &[&str] = &["asset", "liability", "equity", "income", "expense"];

#[derive(Deserialize)]
struct SetupLedger {
    name: String,
    base_currency: String,
}

#[derive(Deserialize)]
struct NewAccount {
    code: String,
    name: String,
    kind: String,
    #[serde(default)]
    parent: Option<String>,
    #[serde(default)]
    is_group: bool,
    #[serde(default)]
    is_control: bool,
}

#[derive(Deserialize)]
struct Code {
    code: String,
}

#[derive(Deserialize)]
struct Year {
    year: i32,
}

#[derive(Deserialize)]
struct NewRule {
    event: String,
    journal: String,
    source_path: String,
    lines: Value,
    #[serde(default)]
    memo: Option<String>,
    #[serde(default)]
    currency_path: Option<String>,
}

#[derive(Deserialize)]
struct Id {
    id: String,
}

fn make_ledger(input: SetupLedger) -> Result<Record> {
    require_admin()?;
    if db::count("gl_ledger", Filter::all())? > 0 {
        return Err(Error::msg("the ledger is already set up"));
    }
    if input.base_currency.trim().is_empty() {
        return Err(Error::msg("name the ledger's currency"));
    }
    db::create("gl_ledger", &json!({ "name": input.name, "base_currency": input.base_currency.trim(), "seq": 0 }))
}

fn new_account(input: NewAccount) -> Result<Record> {
    require_admin()?;
    if !KINDS.contains(&input.kind.as_str()) {
        return Err(Error::msg("kind is asset, liability, equity, income or expense"));
    }
    if input.is_group && input.is_control {
        return Err(Error::msg("a group cannot be a control account"));
    }
    let mut data = json!({ "code": input.code, "name": input.name, "kind": input.kind, "is_group": input.is_group, "is_control": input.is_control, "is_active": true });
    if let Some(parent_code) = &input.parent {
        let parent = account_by_code(parent_code)?;
        if parent.get("is_group") != Some(&json!(true)) {
            return Err(Error::msg("an account sits under a group account"));
        }
        if text(&parent, "kind") != Some(input.kind.as_str()) {
            return Err(Error::msg("an account has the same kind as its group"));
        }
        data["parent"] = parent["id"].clone();
    }
    db::create("gl_account", &data).map_err(|e| e.or("could not create the account (the code may be taken)"))
}

fn retire_account(input: Code) -> Result<Record> {
    require_admin()?;
    let account = account_by_code(&input.code)?;
    // An account is never deleted; one with children still active cannot go quiet under them.
    if db::count("gl_account", Filter::eq("parent", id_of(&account)?).and(Filter::eq("is_active", true)))? > 0 {
        return Err(Error::msg("deactivate the accounts under it first"));
    }
    db::update("gl_account", id_of(&account)?, &json!({ "is_active": false }))?.ok_or_else(|| Error::msg("the account is gone"))
}

fn new_journal(input: Record) -> Result<Record> {
    require_admin()?;
    let mut data = pick(&input, &["code", "name", "kind", "prefix"]);
    data["next"] = json!(0);
    if let Some(code) = text(&input, "default_account") {
        data["default_account"] = account_by_code(code)?["id"].clone();
    }
    db::create("gl_journal", &data).map_err(|e| e.or("could not create the journal (the code or prefix may be taken)"))
}

fn last_day(year: i32, month: u32) -> Result<NaiveDate> {
    let (next_year, next_month) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
    NaiveDate::from_ymd_opt(next_year, next_month, 1)
        .and_then(|d| d.pred_opt())
        .ok_or_else(|| Error::msg("that year has no such month"))
}

fn fiscal_year(input: Year) -> Result<Vec<Record>> {
    require_admin()?;
    if !(1990..=2100).contains(&input.year) {
        return Err(Error::msg("a year between 1990 and 2100"));
    }
    let mut made = Vec::new();
    for month in 1..=12u32 {
        let name = format!("{}-{month:02}", input.year);
        if db::count("gl_period", Filter::eq("name", name.as_str()))? > 0 {
            continue;
        }
        let start = NaiveDate::from_ymd_opt(input.year, month, 1).ok_or_else(|| Error::msg("a bad month"))?;
        made.push(db::create::<Record>(
            "gl_period",
            &json!({ "name": name, "start_date": format_date(start), "end_date": format_date(last_day(input.year, month)?), "status": "open" }),
        )?);
    }
    Ok(made)
}

#[derive(Deserialize)]
struct PeriodName {
    name: String,
}

/// Closing is final, and in order: every earlier period must be closed first.
fn lock_period(input: PeriodName) -> Result<Record> {
    require_admin()?;
    let period = db::find::<Record>("gl_period").filter("name", input.name.as_str()).first()?.ok_or_else(|| Error::msg("there is no such period"))?;
    if text(&period, "status") == Some("closed") {
        return Err(Error::msg("this period is already closed"));
    }
    let start = text(&period, "start_date").unwrap_or_default();
    let earlier_open = db::count("gl_period", Filter::lt("start_date", start).and(Filter::eq("status", "open")))?;
    if earlier_open > 0 {
        return Err(Error::msg("close the earlier periods first"));
    }
    db::update("gl_period", id_of(&period)?, &json!({ "status": "closed", "closed_on": crate::common::today()? }))?.ok_or_else(|| Error::msg("the period is gone"))
}

fn new_rule(input: NewRule) -> Result<Record> {
    require_admin()?;
    let journal = journal_by_code(&input.journal)?;
    let lines = input.lines.as_array().filter(|l| !l.is_empty()).ok_or_else(|| Error::msg("a rule has lines"))?;
    // Every account the rule can name must exist.
    for line in lines {
        let mut codes: Vec<String> = Vec::new();
        if let Some(code) = line.get("account").and_then(Value::as_str) {
            codes.push(code.to_string());
        }
        if let Some(by) = line.get("account_by") {
            if let Some(map) = by.get("map").and_then(Value::as_object) {
                codes.extend(map.values().filter_map(Value::as_str).map(str::to_string));
            }
            if let Some(code) = by.get("default").and_then(Value::as_str) {
                codes.push(code.to_string());
            }
        }
        if codes.is_empty() {
            return Err(Error::msg("a rule line names an account"));
        }
        for code in codes {
            account_by_code(&code)?;
        }
    }
    // The template must be well formed for an empty payload.
    apply_template(&input.lines, &json!({}))?;
    let existing: Vec<Record> = db::find("gl_posting_rule").filter("event", input.event.as_str()).order_by("-version").limit(1).all()?;
    let version = existing.first().and_then(|r| r.get("version").and_then(Value::as_i64)).unwrap_or(0) + 1;
    // A new version replaces the active one for new events; old entries keep the version they used.
    for old in db::find::<Record>("gl_posting_rule").filter("event", input.event.as_str()).filter("is_active", true).limit(50).all()? {
        db::update::<Record>("gl_posting_rule", id_of(&old)?, &json!({ "is_active": false }))?;
    }
    let mut data = json!({
        "event": input.event, "version": version, "is_active": true, "journal": journal["id"], "source_path": input.source_path, "lines": input.lines,
    });
    if let Some(memo) = &input.memo {
        data["memo"] = json!(memo);
    }
    if let Some(path) = &input.currency_path {
        data["currency_path"] = json!(path);
    }
    db::create("gl_posting_rule", &data)
}

handler! {
    fn setup_ledger(input: SetupLedger) -> Record {
        make_ledger(input)
    }

    fn get_ledger(_: Empty) -> Record {
        ledger()
    }

    fn create_account(input: NewAccount) -> Record {
        new_account(input)
    }

    fn deactivate_account(input: Code) -> Record {
        retire_account(input)
    }

    fn list_accounts(_: Empty) -> Vec<Record> {
        crate::common::require_reader()?;
        db::find("gl_account").order_by("code").limit(1000).all()
    }

    fn create_journal(input: Record) -> Record {
        new_journal(input)
    }

    fn list_journals(_: Empty) -> Vec<Record> {
        crate::common::require_reader()?;
        db::find("gl_journal").order_by("code").limit(200).all()
    }

    fn create_fiscal_year(input: Year) -> Vec<Record> {
        fiscal_year(input)
    }

    fn close_period(input: PeriodName) -> Record {
        lock_period(input)
    }

    fn list_periods(_: Empty) -> Vec<Record> {
        crate::common::require_reader()?;
        db::find("gl_period").order_by("start_date").limit(500).all()
    }

    fn create_posting_rule(input: NewRule) -> Record {
        new_rule(input)
    }

    fn deactivate_posting_rule(input: Id) -> Record {
        require_admin()?;
        db::update("gl_posting_rule", &input.id, &json!({ "is_active": false }))?.ok_or_else(|| Error::msg("there is no such rule"))
    }

    fn list_posting_rules(_: Empty) -> Vec<Record> {
        crate::common::require_reader()?;
        db::find("gl_posting_rule").order_by("event").limit(200).all()
    }
}
