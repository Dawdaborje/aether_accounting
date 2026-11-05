//! Helpers shared by the parts of the plugin.

pub use aether_sdk::records::{id_of, require, text, today, Record};

use aether_sdk::dates::{parse_date, NaiveDate};
use aether_sdk::decimal::Decimal;
use aether_sdk::prelude::*;

pub const CLERK_ROLE: &str = "gl_payment.pay_clerk";
pub const ADMIN_ROLE: &str = "gl.gl_admin";
pub const ACCOUNTANT_ROLE: &str = "gl.gl_accountant";
pub const READER_ROLE: &str = "gl.gl_reader";

/// Source kind of the ledger entries this plugin posts (the ledger adds `doc:` in front).
pub const PAYMENT_SOURCE: &str = "gl_payment.payment";

/// Call another plugin, keeping its refusals as messages for the person. The kernel hands a called plugin's
/// failure back as a kernel error, which would reach the person as "plugin invocation failed" without the reason
/// (no period for the date, nothing left to match).
pub fn call<T: serde::de::DeserializeOwned>(plugin: &str, function: &str, input: &impl Serialize) -> Result<T> {
    plugins::call(plugin, function, input).map_err(|error| match error {
        Error::Kernel(message) => Error::msg(message),
        other => other,
    })
}

fn any_role(roles: &[&str]) -> Result<()> {
    let context = context::current()?;
    if roles.iter().any(|r| context.has_role(r)) {
        Ok(())
    } else {
        Err(Error::msg(format!("you need the role `{}` to do this", roles[0])))
    }
}

pub fn require_admin() -> Result<()> {
    any_role(&[ADMIN_ROLE])
}

pub fn require_accountant() -> Result<()> {
    any_role(&[ACCOUNTANT_ROLE, ADMIN_ROLE])
}

pub fn require_clerk() -> Result<()> {
    any_role(&[CLERK_ROLE, ACCOUNTANT_ROLE, ADMIN_ROLE])
}

pub fn require_reader() -> Result<()> {
    any_role(&[READER_ROLE, CLERK_ROLE, ACCOUNTANT_ROLE, ADMIN_ROLE])
}

/// Documents come from bridge plugins, which run as the kernel when an event is delivered; an administrator may
/// also register one by hand (an invoice still open from before the ledger was used).
pub fn require_kernel_or_admin() -> Result<()> {
    let context = context::current()?;
    if !context.is_member() || context.has_role(ADMIN_ROLE) {
        Ok(())
    } else {
        Err(Error::msg(format!("documents are registered by the bridge plugins or by someone with `{ADMIN_ROLE}`")))
    }
}

pub fn actor_name() -> Result<Option<String>> {
    Ok(context::current()?.actor.id)
}

pub fn today_date() -> Result<NaiveDate> {
    parse_date(&today()?)
}

pub fn zero() -> Decimal {
    Decimal::zero(2)
}

pub fn money(amount: Decimal) -> Result<Decimal> {
    amount.with_scale(2)
}

pub fn dec(record: &Record, field: &str) -> Result<Decimal> {
    dec_value(record.get(field).unwrap_or(&Value::Null), field)
}

pub fn dec_value(value: &Value, what: &str) -> Result<Decimal> {
    match value {
        Value::Null => Ok(zero()),
        Value::String(text) => Decimal::parse(text)?.with_scale(2),
        other => serde_json::from_value::<Decimal>(other.clone()).map_err(|e| Error::msg(format!("{what}: {e}")))?.with_scale(2),
    }
}

pub fn settings() -> Result<Record> {
    db::find::<Record>("pay_setting").first()?.ok_or_else(|| Error::msg("payments are not set up yet: an administrator runs setup_payments first"))
}

/// The control account a side of the books is kept on.
pub fn control_account(settings: &Record, side: &str) -> Result<String> {
    let field = match side {
        "receivable" => "receivable_account",
        "payable" => "payable_account",
        _ => return Err(Error::msg("the side is receivable or payable")),
    };
    text(settings, field).map(str::to_string).ok_or_else(|| Error::msg(format!("payments have no {field}")))
}

/// Money in settles what is receivable; money out settles what is payable.
pub fn side_of(direction: &str) -> Result<&'static str> {
    match direction {
        "in" => Ok("receivable"),
        "out" => Ok("payable"),
        _ => Err(Error::msg("the direction is in or out")),
    }
}

pub fn ledger_currency() -> Result<String> {
    let ledger: Record = call("gl", "get_ledger", &json!({}))?;
    Ok(text(&ledger, "base_currency").unwrap_or_default().to_string())
}

/// What the ledger says about lines: open amount and allocations, keyed by line id. Asks 200 at a time.
pub fn line_details(lines: &[String]) -> Result<std::collections::BTreeMap<String, Value>> {
    let mut out = std::collections::BTreeMap::new();
    for chunk in lines.chunks(200) {
        let answer: Value = call("gl", "line_details", &json!({ "lines": chunk }))?;
        for line in answer["lines"].as_array().cloned().unwrap_or_default() {
            if let Some(id) = line["line"].as_str() {
                out.insert(id.to_string(), line.clone());
            }
        }
    }
    Ok(out)
}

/// What is still matched between two lines: allocations and their undoings added up.
pub fn matched_between(detail: &Value, debit_line: &str, credit_line: &str) -> Result<Decimal> {
    let mut sum = zero();
    for row in detail["allocations"].as_array().cloned().unwrap_or_default() {
        if row["debit_line"].as_str() == Some(debit_line) && row["credit_line"].as_str() == Some(credit_line) {
            sum = sum + dec_value(&row["amount"], "amount")?;
        }
    }
    Ok(sum)
}
