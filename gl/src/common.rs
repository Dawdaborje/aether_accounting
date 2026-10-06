//! Helpers shared by the parts of the plugin.

pub use aether_sdk::records::{id_of, pick, require, text, today, Record};

use aether_sdk::dates::{parse_date, NaiveDate};
use aether_sdk::decimal::Decimal;
use aether_sdk::prelude::*;

pub const ADMIN_ROLE: &str = "gl.gl_admin";
pub const ACCOUNTANT_ROLE: &str = "gl.gl_accountant";
pub const READER_ROLE: &str = "gl.gl_reader";

pub fn require_admin() -> Result<()> {
    context::current()?.require_role(ADMIN_ROLE)
}

pub fn require_accountant() -> Result<()> {
    let context = context::current()?;
    if context.has_role(ACCOUNTANT_ROLE) || context.has_role(ADMIN_ROLE) {
        Ok(())
    } else {
        Err(Error::msg(format!("you need the role `{ACCOUNTANT_ROLE}` to do this")))
    }
}

pub fn require_reader() -> Result<()> {
    let context = context::current()?;
    if [READER_ROLE, ACCOUNTANT_ROLE, ADMIN_ROLE].iter().any(|r| context.has_role(r)) {
        Ok(())
    } else {
        Err(Error::msg(format!("you need the role `{READER_ROLE}` to do this")))
    }
}

pub fn today_date() -> Result<NaiveDate> {
    parse_date(&today()?)
}

pub fn actor_name() -> Result<Option<String>> {
    Ok(context::current()?.actor.id)
}

pub fn ledger() -> Result<Record> {
    db::find::<Record>("gl_ledger").first()?.ok_or_else(|| Error::msg("the ledger is not set up yet: an administrator runs setup_ledger first"))
}

pub fn account_by_code(code: &str) -> Result<Record> {
    db::find::<Record>("gl_account").filter("code", code).first()?.ok_or_else(|| Error::msg(format!("there is no account `{code}`")))
}

pub fn journal_by_code(code: &str) -> Result<Record> {
    db::find::<Record>("gl_journal").filter("code", code).first()?.ok_or_else(|| Error::msg(format!("there is no journal `{code}`")))
}

pub fn dec(record: &Record, field: &str) -> Result<Decimal> {
    match record.get(field).filter(|v| !v.is_null()) {
        Some(value) => serde_json::from_value(value.clone()).map_err(|e| Error::msg(format!("{field}: {e}"))),
        None => Ok(Decimal::zero(2)),
    }
}

/// An amount the way an aggregate answers: text, or a number of whole units of the smallest digit.
pub fn amount_of(value: &Value) -> Result<Decimal> {
    match value {
        Value::String(text) => Decimal::parse(text)?.with_scale(2),
        Value::Number(number) => {
            let units = number.as_i64().ok_or_else(|| Error::msg("an amount is not a whole number of cents"))?;
            let sign = if units < 0 { "-" } else { "" };
            Decimal::parse(&format!("{sign}{}.{:02}", units.abs() / 100, units.abs() % 100))
        }
        Value::Null => Ok(Decimal::zero(2)),
        _ => Err(Error::msg("an amount is not text or a number")),
    }
}
