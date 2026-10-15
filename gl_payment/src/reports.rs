//! What is open, how late it is, and what happened with a party. Open amounts come from the ledger.

use aether_sdk::dates::parse_date;
use aether_sdk::db::Filter;
use aether_sdk::decimal::Decimal;
use aether_sdk::prelude::*;

use crate::common::{dec, require_reader, text, today_date, zero, Record};
use crate::documents::open_documents;

#[derive(Deserialize, Default)]
struct Outstanding {
    side: String,
    #[serde(default)]
    party: Option<String>,
    #[serde(default)]
    company: Option<String>,
    /// The day lateness is counted to; today by default.
    #[serde(default)]
    as_of: Option<String>,
}

#[derive(Deserialize)]
struct Statement {
    party: String,
    side: String,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
}

#[derive(Deserialize, Default)]
struct Register {
    #[serde(default)]
    direction: Option<String>,
    #[serde(default)]
    party: Option<String>,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    state: Option<String>,
}

/// Aging buckets by days past due: not due yet, then 1–30, 31–60, 61–90 and over 90.
const BUCKETS: [&str; 5] = ["current", "1_30", "31_60", "61_90", "over_90"];

pub fn bucket(days_late: i64) -> usize {
    match days_late {
        i64::MIN..=0 => 0,
        1..=30 => 1,
        31..=60 => 2,
        61..=90 => 3,
        _ => 4,
    }
}

fn scope(input: &Outstanding) -> Result<Filter> {
    if !matches!(input.side.as_str(), "receivable" | "payable") {
        return Err(Error::msg("the side is receivable or payable"));
    }
    let mut filter = Filter::eq("side", input.side.as_str());
    if let Some(party) = &input.party {
        filter = filter.and(Filter::eq("party", party.as_str()));
    }
    if let Some(company) = &input.company {
        filter = filter.and(Filter::eq("company", company.as_str()));
    }
    Ok(filter)
}

fn as_of(input: &Outstanding) -> Result<aether_sdk::dates::NaiveDate> {
    match &input.as_of {
        Some(day) => parse_date(day),
        None => today_date(),
    }
}

fn outstanding(input: Outstanding) -> Result<Value> {
    require_reader()?;
    let day = as_of(&input)?;
    let (documents, truncated) = open_documents(scope(&input)?)?;
    let mut rows = Vec::new();
    let mut total = zero();
    for (document, open) in documents {
        let due = parse_date(text(&document, "due_date").unwrap_or_default())?;
        let late = (day - due).num_days();
        total = total + open;
        rows.push(json!({
            "document": document["id"], "number": document["number"], "party": document["party"], "company": document["company"],
            "date": document["date"], "due_date": document["due_date"], "total": document["total"], "open": open,
            "days_late": late.max(0), "linked": text(&document, "gl_line").is_some(),
        }));
    }
    Ok(json!({ "documents": rows, "total_open": total, "as_of": aether_sdk::dates::format_date(day), "truncated": truncated }))
}

fn aging(input: Outstanding) -> Result<Value> {
    require_reader()?;
    let day = as_of(&input)?;
    let (documents, truncated) = open_documents(scope(&input)?)?;
    let mut parties: std::collections::BTreeMap<String, [Decimal; 5]> = std::collections::BTreeMap::new();
    let mut totals = [zero(); 5];
    for (document, open) in documents {
        let due = parse_date(text(&document, "due_date").unwrap_or_default())?;
        let index = bucket((day - due).num_days());
        let party = parties.entry(text(&document, "party").unwrap_or_default().to_string()).or_insert([zero(); 5]);
        party[index] = party[index] + open;
        totals[index] = totals[index] + open;
    }
    let row = |figures: &[Decimal; 5]| {
        let mut out = json!({});
        let mut sum = zero();
        for (name, amount) in BUCKETS.iter().zip(figures) {
            out[*name] = json!(amount);
            sum = sum + *amount;
        }
        out["total"] = json!(sum);
        out
    };
    let rows: Vec<Value> = parties
        .iter()
        .map(|(party, figures)| {
            let mut out = row(figures);
            out["party"] = json!(party);
            out
        })
        .collect();
    Ok(json!({ "side": input.side, "as_of": aether_sdk::dates::format_date(day), "parties": rows, "total": row(&totals), "truncated": truncated }))
}

/// Documents raise what the party owes (or is owed); payments lower it. Cancelled payments and void documents
/// are left out. The balance before `from` is the opening line.
fn statement(input: Statement) -> Result<Value> {
    require_reader()?;
    let direction = match input.side.as_str() {
        "receivable" => "in",
        "payable" => "out",
        _ => return Err(Error::msg("the side is receivable or payable")),
    };
    let documents: Vec<Record> = db::find("pay_document")
        .matching(Filter::eq("party", input.party.as_str()).and(Filter::eq("side", input.side.as_str())).and(Filter::ne("state", "void")))
        .order_by("date")
        .limit(2000)
        .all()?;
    let payments: Vec<Record> = db::find("payment")
        .matching(Filter::eq("party", input.party.as_str()).and(Filter::eq("direction", direction)).and(Filter::eq("state", "confirmed")))
        .order_by("date")
        .limit(2000)
        .all()?;
    let mut moves: Vec<(String, Value, Decimal)> = Vec::new();
    for document in &documents {
        let amount = dec(document, "total")?;
        moves.push((text(document, "date").unwrap_or_default().to_string(), json!({ "kind": "document", "number": document["number"], "due_date": document["due_date"] }), amount));
    }
    for payment in &payments {
        let amount = dec(payment, "amount")?;
        moves.push((text(payment, "date").unwrap_or_default().to_string(), json!({ "kind": "payment", "number": payment["number"], "reference": payment["reference"] }), zero() - amount));
    }
    moves.sort_by(|a, b| a.0.cmp(&b.0));
    let mut opening = zero();
    let mut balance = zero();
    let mut lines = Vec::new();
    for (date, mut line, amount) in moves {
        if input.from.as_deref().is_some_and(|from| date.as_str() < from) {
            opening = opening + amount;
            balance = balance + amount;
            continue;
        }
        if input.to.as_deref().is_some_and(|to| date.as_str() > to) {
            continue;
        }
        balance = balance + amount;
        line["date"] = json!(date);
        line["amount"] = json!(amount);
        line["balance"] = json!(balance);
        lines.push(line);
    }
    Ok(json!({ "party": input.party, "side": input.side, "opening": opening, "lines": lines, "closing": balance }))
}

fn register(input: Register) -> Result<Vec<Record>> {
    require_reader()?;
    let mut filter = Filter::all();
    if let Some(direction) = &input.direction {
        filter = filter.and(Filter::eq("direction", direction.as_str()));
    }
    if let Some(party) = &input.party {
        filter = filter.and(Filter::eq("party", party.as_str()));
    }
    if let Some(state) = &input.state {
        filter = filter.and(Filter::eq("state", state.as_str()));
    }
    if let Some(from) = &input.from {
        filter = filter.and(Filter::gte("date", from.as_str()));
    }
    if let Some(to) = &input.to {
        filter = filter.and(Filter::lte("date", to.as_str()));
    }
    db::find("payment").matching(filter).order_by("-date").limit(500).all()
}

handler! {
    /// Open documents on one side, oldest due first, with how many days late each is.
    fn outstanding_documents(input: Outstanding) -> Value {
        outstanding(input)
    }

    /// Open amounts per party in buckets by days past due.
    fn aging_report(input: Outstanding) -> Value {
        aging(input)
    }

    /// Documents and payments of one party with a running balance.
    fn party_statement(input: Statement) -> Value {
        statement(input)
    }

    /// Payments, newest first.
    fn payment_register(input: Option<Register>) -> Vec<Record> {
        register(input.unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_by_days_late() {
        assert_eq!(bucket(-5), 0);
        assert_eq!(bucket(0), 0);
        assert_eq!(bucket(1), 1);
        assert_eq!(bucket(30), 1);
        assert_eq!(bucket(31), 2);
        assert_eq!(bucket(90), 3);
        assert_eq!(bucket(91), 4);
    }
}
