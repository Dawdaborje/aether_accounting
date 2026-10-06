//! The rules of the ledger, with nothing read from a database.
//!
//! What the ERPNext and Odoo sources taught (see `docs/design.md`): an entry balances exactly, in one
//! place, with no tolerance and no automatic round-off line (ERPNext adds one and exempts one entry
//! type); an entry is written once and corrected by a reversal; numbers come from a counter, not from
//! parsing a name; open amounts are derived from allocation rows, never stored on a line.

use aether_sdk::dates::NaiveDate;
use aether_sdk::decimal::Decimal;
use aether_sdk::{json, Error, Result, Value};
use sha2::{Digest, Sha256};

/// A transaction holds 50 writes; an entry takes one, the counters two.
pub const MAX_LINES: usize = 45;
pub const HASH_VERSION: i64 = 1;
pub const GENESIS: &str = "genesis";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub account: String,
    pub debit: Decimal,
    pub credit: Decimal,
    pub party: Option<String>,
    pub tax_code: Option<String>,
    pub memo: Option<String>,
}

fn zero() -> Decimal {
    Decimal::zero(2)
}

/// An entry's lines are valid and balance exactly; returns the total (of the debits).
pub fn check_lines(lines: &[Line]) -> Result<Decimal> {
    if lines.len() < 2 {
        return Err(Error::msg("an entry has at least two lines"));
    }
    if lines.len() > MAX_LINES {
        return Err(Error::msg(format!("an entry has at most {MAX_LINES} lines: split it")));
    }
    let (mut debits, mut credits) = (zero(), zero());
    for (index, line) in lines.iter().enumerate() {
        let number = index + 1;
        if line.debit < zero() || line.credit < zero() {
            return Err(Error::msg(format!("line {number}: an amount is not negative; use the other side")));
        }
        match (line.debit > zero(), line.credit > zero()) {
            (true, false) => debits = debits + line.debit,
            (false, true) => credits = credits + line.credit,
            (true, true) => return Err(Error::msg(format!("line {number}: a line is a debit or a credit, not both"))),
            (false, false) => return Err(Error::msg(format!("line {number}: a line has an amount"))),
        }
    }
    if debits != credits {
        return Err(Error::msg(format!("the entry does not balance: debits {debits}, credits {credits}")));
    }
    Ok(debits)
}

/// Lines with the same account, party, tax code and side are one line.
pub fn merge_lines(lines: Vec<Line>) -> Vec<Line> {
    let mut merged: Vec<Line> = Vec::new();
    for line in lines {
        let same = merged.iter_mut().find(|m| {
            m.account == line.account
                && m.party == line.party
                && m.tax_code == line.tax_code
                && (m.debit > zero()) == (line.debit > zero())
        });
        match same {
            Some(existing) => {
                existing.debit = existing.debit + line.debit;
                existing.credit = existing.credit + line.credit;
            }
            None => merged.push(line),
        }
    }
    merged
}

pub fn numbered(prefix: &str, year: i32, n: i64) -> String {
    format!("{prefix}-{year}-{n:05}")
}

/// The text an entry's hash covers: the previous hash, and the entry's own fields and lines, in a fixed order.
pub fn canonical(prev_hash: &str, seq: i64, number: &str, date: &str, journal: &str, currency: &str, lines: &[(String, &Line)]) -> String {
    let mut text = format!("v{HASH_VERSION}|{prev_hash}|{seq}|{number}|{date}|{journal}|{currency}");
    for (code, line) in lines {
        text.push_str(&format!(
            "\n{code}|{}|{}|{}|{}",
            line.debit,
            line.credit,
            line.party.as_deref().unwrap_or(""),
            line.tax_code.as_deref().unwrap_or("")
        ));
    }
    text
}

pub fn sha256_hex(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// The period that holds a day.
pub fn period_for<'a, T>(date: NaiveDate, periods: &'a [(NaiveDate, NaiveDate, T)]) -> Option<&'a T> {
    periods.iter().find(|(start, end, _)| *start <= date && date <= *end).map(|(_, _, t)| t)
}

/// What is still open on a line: its amount less what has been allocated (undone allocations are negative rows).
pub fn open_amount(amount: Decimal, allocations: &[Decimal]) -> Decimal {
    allocations.iter().fold(amount, |open, a| open - *a)
}

/// Match debits to credits, oldest first, each only as far as both have room.
pub fn fifo(debits: &[(String, Decimal)], credits: &[(String, Decimal)]) -> Vec<(String, String, Decimal)> {
    let mut out = Vec::new();
    let mut left_credits: Vec<(String, Decimal)> = credits.iter().filter(|(_, a)| *a > zero()).cloned().collect();
    for (debit_id, debit_open) in debits.iter().filter(|(_, a)| *a > zero()) {
        let mut room = *debit_open;
        for (credit_id, credit_open) in left_credits.iter_mut() {
            if room <= zero() {
                break;
            }
            if *credit_open <= zero() {
                continue;
            }
            let part = room.min(*credit_open);
            out.push((debit_id.clone(), credit_id.clone(), part));
            room = room - part;
            *credit_open = *credit_open - part;
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub account: String,
    pub debit: bool,
    pub amount: Decimal,
    pub party: Option<String>,
}

/// A value in a payload at a dotted path (`a.b.c`).
pub fn dig<'a>(payload: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(payload, |value, key| value.get(key))
}

fn amount_at(item: &Value, path: &str) -> Result<Decimal> {
    match dig(item, path) {
        Some(Value::String(text)) => Decimal::parse(text)?.with_scale(2),
        Some(Value::Number(number)) if number.is_i64() => Decimal::whole(number.as_i64().unwrap_or(0), 2),
        Some(Value::Number(_)) => Err(Error::msg(format!("`{path}` is a fractional number: events send amounts as text"))),
        Some(Value::Null) | None => Ok(zero()),
        Some(_) => Err(Error::msg(format!("`{path}` is not an amount"))),
    }
}

fn text_at(item: &Value, path: &str) -> Option<String> {
    match dig(item, path) {
        Some(Value::String(t)) if !t.is_empty() => Some(t.clone()),
        _ => None,
    }
}

/// Lines from a posting rule's template and an event's payload. A template line is
/// `{ account, side, amount, party?, each?, where?: { field, equals }, account_by?: { field, map, default } }`:
/// `amount` and `party` are paths (inside the item, for `each`); `where` keeps only matching items. Zero amounts are dropped.
pub fn apply_template(template: &Value, payload: &Value) -> Result<Vec<Resolved>> {
    let lines = template.as_array().ok_or_else(|| Error::msg("a posting rule's lines are a list"))?;
    let mut out = Vec::new();
    for line in lines {
        let side = match line.get("side").and_then(Value::as_str) {
            Some("debit") => true,
            Some("credit") => false,
            _ => return Err(Error::msg("a rule line's side is debit or credit")),
        };
        let amount_path = line.get("amount").and_then(Value::as_str).ok_or_else(|| Error::msg("a rule line names where its amount is"))?;
        let party_path = line.get("party").and_then(Value::as_str);
        let items: Vec<&Value> = match line.get("each").and_then(Value::as_str) {
            Some(path) => dig(payload, path).and_then(Value::as_array).map(|a| a.iter().collect()).unwrap_or_default(),
            None => vec![payload],
        };
        for item in items {
            // `where: { field, equals }` keeps only the items whose field has that text.
            if let Some(filter) = line.get("where") {
                let field = filter.get("field").and_then(Value::as_str).ok_or_else(|| Error::msg("where names a field"))?;
                let wanted = filter.get("equals").and_then(Value::as_str).ok_or_else(|| Error::msg("where says what it equals"))?;
                if text_at(item, field).as_deref() != Some(wanted) {
                    continue;
                }
            }
            let account = match line.get("account_by") {
                Some(by) => {
                    let field = by.get("field").and_then(Value::as_str).ok_or_else(|| Error::msg("account_by names a field"))?;
                    let key = text_at(item, field).unwrap_or_default();
                    by.get("map")
                        .and_then(|m| m.get(&key))
                        .or_else(|| by.get("default"))
                        .and_then(Value::as_str)
                        .ok_or_else(|| Error::msg(format!("no account for `{key}` and no default")))?
                        .to_string()
                }
                None => line.get("account").and_then(Value::as_str).ok_or_else(|| Error::msg("a rule line names an account"))?.to_string(),
            };
            let amount = amount_at(item, amount_path)?;
            if amount == zero() {
                continue;
            }
            // The party of an item may be named on the item or on the whole payload.
            let party = party_path.and_then(|p| text_at(item, p).or_else(|| text_at(payload, p)));
            out.push(Resolved { account, debit: side, amount, party });
        }
    }
    Ok(out)
}

pub fn line_json(code: &str, line: &Line) -> Value {
    json!({ "account": code, "debit": line.debit, "credit": line.credit, "party": line.party, "tax_code": line.tax_code, "memo": line.memo })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_sdk::dates::parse_date;

    fn d(text: &str) -> Result<Decimal> {
        Decimal::parse(text)
    }

    fn line(account: &str, debit: &str, credit: &str) -> Result<Line> {
        Ok(Line { account: account.into(), debit: d(debit)?, credit: d(credit)?, party: None, tax_code: None, memo: None })
    }

    #[test]
    fn an_entry_balances_exactly() -> Result<()> {
        assert_eq!(check_lines(&[line("a", "100.00", "0.00")?, line("b", "0.00", "100.00")?])?, d("100.00")?);
        assert!(check_lines(&[line("a", "100.00", "0.00")?, line("b", "0.00", "99.99")?]).is_err(), "no tolerance, no round-off");
        assert!(check_lines(&[line("a", "100.00", "0.00")?]).is_err());
        assert!(check_lines(&[line("a", "50.00", "50.00")?, line("b", "0.00", "0.00")?]).is_err());
        assert!(check_lines(&[line("a", "0.00", "0.00")?, line("b", "0.00", "0.00")?]).is_err());
        assert!(check_lines(&[line("a", "-5.00", "0.00")?, line("b", "0.00", "-5.00")?]).is_err());
        let many: Vec<Line> = (0..46).map(|i| line("a", if i % 2 == 0 { "1.00" } else { "0.00" }, if i % 2 == 0 { "0.00" } else { "1.00" })).collect::<Result<_>>()?;
        assert!(check_lines(&many).is_err());
        Ok(())
    }

    #[test]
    fn lines_on_the_same_account_merge_by_side() -> Result<()> {
        let mut with_party = line("exp", "10.00", "0.00")?;
        with_party.party = Some("e1".into());
        let merged = merge_lines(vec![line("exp", "10.00", "0.00")?, line("exp", "5.50", "0.00")?, line("pay", "0.00", "15.50")?, with_party]);
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0].debit, d("15.50")?);
        Ok(())
    }

    #[test]
    fn numbers_and_hashes_are_stable() -> Result<()> {
        assert_eq!(numbered("GJ", 2026, 7), "GJ-2026-00007");
        let l = line("1000", "10.00", "0.00")?;
        let text = canonical(GENESIS, 1, "GJ-2026-00001", "2026-10-06", "GJ", "GMD", &[("1000".into(), &l)]);
        assert_eq!(sha256_hex(&text), sha256_hex(&text));
        assert_eq!(sha256_hex("abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        let other = canonical(GENESIS, 1, "GJ-2026-00001", "2026-10-06", "GJ", "GMD", &[("1000".into(), &line("1000", "10.01", "0.00")?)]);
        assert_ne!(sha256_hex(&text), sha256_hex(&other), "an amount is part of the hash");
        Ok(())
    }

    #[test]
    fn a_period_holds_a_day() -> Result<()> {
        let periods = vec![(parse_date("2026-01-01")?, parse_date("2026-01-31")?, "jan"), (parse_date("2026-02-01")?, parse_date("2026-02-28")?, "feb")];
        assert_eq!(period_for(parse_date("2026-02-15")?, &periods), Some(&"feb"));
        assert_eq!(period_for(parse_date("2026-03-01")?, &periods), None);
        Ok(())
    }

    #[test]
    fn open_amounts_come_from_allocations() -> Result<()> {
        assert_eq!(open_amount(d("100.00")?, &[d("30.00")?, d("20.00")?]), d("50.00")?);
        assert_eq!(open_amount(d("100.00")?, &[d("30.00")?, d("-30.00")?]), d("100.00")?, "an undone allocation gives the room back");
        Ok(())
    }

    #[test]
    fn matching_goes_oldest_first_as_far_as_both_have_room() -> Result<()> {
        let debits = vec![("d1".to_string(), d("100.00")?), ("d2".to_string(), d("50.00")?)];
        let credits = vec![("c1".to_string(), d("120.00")?), ("c2".to_string(), d("100.00")?)];
        let pairs = fifo(&debits, &credits);
        assert_eq!(
            pairs,
            vec![
                ("d1".to_string(), "c1".to_string(), d("100.00")?),
                ("d2".to_string(), "c1".to_string(), d("20.00")?),
                ("d2".to_string(), "c2".to_string(), d("30.00")?)
            ]
        );
        assert!(fifo(&debits, &[]).is_empty());
        Ok(())
    }

    fn payload() -> Value {
        json!({
            "employee": "emp1", "approved_total": "1500.00", "advance_applied_total": "200.00", "net_payable": "1300.00",
            "lines": [ { "category": "MEAL", "approved": "300.00" }, { "category": "HOTEL", "approved": "1000.00" }, { "category": "OTHER", "approved": "200.00" }, { "category": "TAXI", "approved": "0.00" } ]
        })
    }

    #[test]
    fn a_rule_turns_a_payload_into_lines() -> Result<()> {
        let template = json!([
            { "side": "debit", "each": "lines", "amount": "approved", "account_by": { "field": "category", "map": { "MEAL": "6100", "HOTEL": "6200" }, "default": "6900" } },
            { "side": "credit", "account": "2100", "amount": "net_payable", "party": "employee" },
            { "side": "credit", "account": "1300", "amount": "advance_applied_total", "party": "employee" }
        ]);
        let resolved = apply_template(&template, &payload())?;
        assert_eq!(resolved.len(), 5, "the zero taxi line is dropped");
        assert_eq!((resolved[0].account.as_str(), resolved[0].amount), ("6100", d("300.00")?));
        assert_eq!(resolved[2].account, "6900", "the default account");
        assert_eq!(resolved[3].party.as_deref(), Some("emp1"));
        let lines: Vec<Line> = resolved
            .iter()
            .map(|r| Line { account: r.account.clone(), debit: if r.debit { r.amount } else { zero() }, credit: if r.debit { zero() } else { r.amount }, party: r.party.clone(), tax_code: None, memo: None })
            .collect();
        assert_eq!(check_lines(&merge_lines(lines))?, d("1500.00")?);
        Ok(())
    }

    #[test]
    fn where_keeps_only_matching_items() -> Result<()> {
        let payload = json!({ "components": [
            { "id": "base", "kind": "earning", "amount": "1000.00" },
            { "id": "paye", "kind": "deduction", "amount": "100.00" },
            { "id": "nssf", "kind": "deduction", "amount": "50.00" },
            { "id": "gross", "kind": "info", "amount": "1000.00" }
        ] });
        let template = json!([
            { "side": "debit", "each": "components", "where": { "field": "kind", "equals": "earning" }, "amount": "amount", "account_by": { "field": "id", "map": { "base": "6000" }, "default": "6900" } },
            { "side": "credit", "each": "components", "where": { "field": "kind", "equals": "deduction" }, "amount": "amount", "account_by": { "field": "id", "map": { "paye": "2210", "nssf": "2220" } } }
        ]);
        let resolved = apply_template(&template, &payload)?;
        assert_eq!(resolved.iter().map(|r| (r.account.as_str(), r.debit)).collect::<Vec<_>>(), vec![("6000", true), ("2210", false), ("2220", false)]);
        Ok(())
    }

    #[test]
    fn a_rule_refuses_what_it_cannot_read() -> Result<()> {
        let no_default = json!([{ "side": "debit", "each": "lines", "amount": "approved", "account_by": { "field": "category", "map": {} } }]);
        assert!(apply_template(&no_default, &payload()).is_err());
        let fraction = json!([{ "side": "debit", "account": "1", "amount": "x" }]);
        assert!(apply_template(&fraction, &json!({ "x": 1.5 })).is_err(), "amounts arrive as text");
        assert!(apply_template(&json!([{ "side": "left", "account": "1", "amount": "x" }]), &json!({})).is_err());
        assert!(apply_template(&json!("lines"), &json!({})).is_err());
        let precise = json!([{ "side": "debit", "account": "1", "amount": "x" }]);
        assert!(apply_template(&precise, &json!({ "x": "1.005" })).is_err(), "no silent rounding");
        Ok(())
    }
}
