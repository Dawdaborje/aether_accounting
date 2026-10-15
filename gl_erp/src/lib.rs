//! Hands stock and purchase events from ERP to the ledger.

use aether_sdk::events::Received;
use aether_sdk::prelude::*;

fn should_skip(received: &Received) -> bool {
    received
        .payload
        .get("kind")
        .and_then(Value::as_str)
        .map(|kind| kind == "transfer")
        .unwrap_or(false)
}

/// An approved invoice or bill as a document `gl_payment` keeps open until it is paid; none for stock events.
fn as_document(received: &Received) -> Option<Value> {
    let p = &received.payload;
    let (side, id, party, date) = match received.event.as_str() {
        "erp_selling.sales_invoice_approved" => ("receivable", "invoice", "customer", "invoice_date"),
        "erp_procurement.purchase_bill_approved" => ("payable", "bill", "supplier", "bill_date"),
        _ => return None,
    };
    Some(json!({
        "side": side, "source_kind": received.event, "source_id": p[id], "version": p["source_version"], "number": p["number"],
        "party": p[party], "company": p["company"], "currency": p["currency"], "date": p[date], "total": p["total"],
        "due_date": p.get("due_date"),
    }))
}

handler! {
    /// Delivered by the kernel for each listened event. Transfers are not posted. Invoices and bills are also
    /// registered with `gl_payment`; both calls are safe to repeat, so a failed delivery is simply tried again.
    fn forward(received: Received) -> Value {
        if should_skip(&received) {
            return Ok(json!({ "posted": false, "skipped": "transfer" }));
        }
        let posted: Value = plugins::call(
            "gl",
            "on_posting_event",
            &json!({
                "event": received.event,
                "source": received.source,
                "payload": received.payload,
                "emitted_by": received.emitted_by,
                "depth": received.depth,
            }),
        )?;
        if let Some(document) = as_document(&received) {
            let registered: Value = plugins::call("gl_payment", "register_document", &document)?;
            return Ok(json!({ "posted": posted, "document": registered["id"] }));
        }
        Ok(posted)
    }

    /// Example posting-rule lines for stock events (account codes are placeholders).
    /// An administrator creates journals/accounts, then `gl.create_posting_rule` with these templates.
    fn stock_rule_templates(_: Empty) -> Value {
        Ok(json!({
            "events": {
                "erp_inventory.stock_posted": {
                    "source_path": "move",
                    "version_path": "source_version",
                    "date_path": "posting_date",
                    "currency_path": "currency",
                    "memo": "Stock move",
                    "lines": [
                        { "side": "debit", "account": "1400", "amount": "inventory_in" },
                        { "side": "credit", "account": "1400", "amount": "inventory_out" },
                        { "side": "credit", "account": "2000", "amount": "clearing" },
                        { "side": "debit", "account": "2000", "amount": "clearing_debit" },
                        { "side": "debit", "account": "5000", "amount": "cogs" },
                        { "side": "credit", "account": "5000", "amount": "cogs_credit" },
                        { "side": "credit", "account": "5100", "amount": "adjust_in" },
                        { "side": "debit", "account": "5100", "amount": "adjust_out" },
                        { "side": "debit", "account": "5200", "amount": "variance_debit" },
                        { "side": "credit", "account": "5200", "amount": "variance_credit" },
                        { "side": "debit", "account": "1450", "amount": "wip_debit" },
                        { "side": "credit", "account": "1450", "amount": "wip_credit" }
                    ],
                    "accounts": {
                        "1400": "Inventory",
                        "1450": "Work in progress",
                        "2000": "Goods received not invoiced",
                        "5000": "Cost of goods sold",
                        "5100": "Inventory adjustment",
                        "5200": "Purchase price variance"
                    }
                },
                "erp_inventory.stock_voided": {
                    "source_path": "move",
                    "version_path": "source_version",
                    "date_path": "posting_date",
                    "currency_path": "currency",
                    "memo": "Stock move voided",
                    "lines": [
                        { "side": "debit", "account": "1400", "amount": "inventory_in" },
                        { "side": "credit", "account": "1400", "amount": "inventory_out" },
                        { "side": "credit", "account": "2000", "amount": "clearing" },
                        { "side": "debit", "account": "2000", "amount": "clearing_debit" },
                        { "side": "debit", "account": "5000", "amount": "cogs" },
                        { "side": "credit", "account": "5000", "amount": "cogs_credit" },
                        { "side": "credit", "account": "5100", "amount": "adjust_in" },
                        { "side": "debit", "account": "5100", "amount": "adjust_out" },
                        { "side": "debit", "account": "5200", "amount": "variance_debit" },
                        { "side": "credit", "account": "5200", "amount": "variance_credit" },
                        { "side": "debit", "account": "1450", "amount": "wip_debit" },
                        { "side": "credit", "account": "1450", "amount": "wip_credit" }
                    ]
                }
            },
            "note": "Create a stock journal, map the account codes to your chart, then call gl.create_posting_rule. Zero amounts are dropped, so one rule covers receipt, issue, adjust, and manufacture."
        }))
    }

    /// Purchase bill approval: clear GRNI and credit AP (placeholder account codes).
    fn bill_rule_templates(_: Empty) -> Value {
        Ok(json!({
            "events": {
                "erp_procurement.purchase_bill_approved": {
                    "source_path": "bill",
                    "version_path": "source_version",
                    "date_path": "bill_date",
                    "currency_path": "currency",
                    "memo": "Purchase bill",
                    "lines": [
                        { "side": "debit", "account": "2000", "amount": "grni_clear" },
                        { "side": "credit", "account": "2100", "amount": "ap_total", "party": "supplier" },
                        { "side": "debit", "account": "5200", "amount": "variance" }
                    ],
                    "accounts": {
                        "2000": "Goods received not invoiced",
                        "2100": "Accounts payable",
                        "5200": "Purchase price variance"
                    }
                }
            },
            "note": "Create an AP journal, map codes to your chart (2100 must be a control account), then call gl.create_posting_rule. When bill total equals GRNI, variance is zero and that line is dropped."
        }))
    }

    /// Sales invoice approval: debit AR / credit revenue (COGS already posted on stock issue).
    fn invoice_rule_templates(_: Empty) -> Value {
        Ok(json!({
            "events": {
                "erp_selling.sales_invoice_approved": {
                    "source_path": "invoice",
                    "version_path": "source_version",
                    "date_path": "invoice_date",
                    "currency_path": "currency",
                    "memo": "Sales invoice",
                    "lines": [
                        { "side": "debit", "account": "1100", "amount": "ar_total", "party": "customer" },
                        { "side": "credit", "account": "4000", "amount": "revenue" }
                    ],
                    "accounts": {
                        "1100": "Accounts receivable",
                        "4000": "Sales revenue"
                    }
                }
            },
            "note": "Create an AR journal, map codes to your chart (1100 must be a control account), then call gl.create_posting_rule. Inventory/COGS come from erp_inventory.stock_posted on delivery."
        }))
    }
}
