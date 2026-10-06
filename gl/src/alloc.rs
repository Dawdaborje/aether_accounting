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
    });
    if let Some(who) = actor_name()? {
        data["by"] = json!(who);
    }
    db::create("gl_allocation", &data)
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
    });
    if let Some(who) = actor_name()? {
        data["by"] = json!(who);
    }
    db::create("gl_allocation", &data)
}

/// Lines on an account (and party) with something still open.
fn open_items(input: &Items) -> Result<Vec<Value>> {
    let account = account_by_code(&input.account)?;
    let mut find = db::find::<Record>("gl_line").filter("account", id_of(&account)?).order_by("date").limit(1000);
    if let Some(party) = &input.party {
        find = find.filter("party", party.as_str());
    }
    let mut out = Vec::new();
    for line in find.all()? {
        let debit_side = dec(&line, "debit")? > zero();
        let open = open_of(&line, debit_side)?;
        if open > zero() {
            out.push(json!({
                "line": line["id"], "entry": line["entry"], "date": line["date"], "party": line["party"],
                "side": if debit_side { "debit" } else { "credit" }, "amount": if debit_side { &line["debit"] } else { &line["credit"] }, "open": open,
            }));
        }
    }
    Ok(out)
}

fn auto_allocate(input: Items) -> Result<Value> {
    require_accountant()?;
    if input.party.is_none() {
        return Err(Error::msg("name the party"));
    }
    let items = open_items(&input)?;
    let mut debits = Vec::new();
    let mut credits = Vec::new();
    for item in &items {
        let id = item["line"].as_str().unwrap_or_default().to_string();
        let open: Decimal = serde_json::from_value(item["open"].clone()).map_err(|e| Error::msg(format!("open: {e}")))?;
        if item["side"] == "debit" { debits.push((id, open)) } else { credits.push((id, open)) }
    }
    let pairs = fifo(&debits, &credits);
    let account = account_by_code(&input.account)?;
    let mut made = 0u64;
    for chunk in pairs.chunks(40) {
        let mut tx = db::transaction();
        for (debit_line, credit_line, amount) in chunk {
            tx = tx.create(
                "gl_allocation",
                &json!({
                    "debit_line": debit_line, "credit_line": credit_line, "account": account["id"], "party": input.party,
                    "amount": amount, "date": format_date(today_date()?),
                }),
            );
        }
        tx.run()?;
        made += chunk.len() as u64;
    }
    Ok(json!({ "allocations": made }))
}

handler! {
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
    fn open_items_report(input: Items) -> Vec<Value> {
        require_reader()?;
        open_items(&input)
    }
}
