# Ledger entries from ERP (`gl_erp`)

## Purpose

Thin bridge from ERP stock, purchasing, and selling into the general ledger. Listens to inventory/procurement/selling events and forwards them to `gl.on_posting_event`. Transfers are skipped (no P&L or balance-sheet change inside one company). Install only when accounting and ERP are both in use.

## Dependencies

| Dependency | Why |
|---|---|
| `gl` | Posts via `plugins::call("gl", "on_posting_event", …)` |
| `erp_inventory` | `erp_inventory.stock_posted`, `erp_inventory.stock_voided` |
| `erp_procurement` | `erp_procurement.purchase_bill_approved` |
| `erp_selling` | `erp_selling.sales_invoice_approved` |

Capabilities: `events::subscribe`, `plugins::call`. No DB access.

## Models

None. No `access_models`, no local tables. Example account codes in rule templates are placeholders for the org’s chart in `gl`.

## Design

- **Event-driven bridge**: same pattern as `gl_hrms` — listen, optionally skip, forward.
- **Transfers skipped**: if payload `kind == "transfer"`, returns `{ posted: false, skipped: "transfer" }` without calling `gl`.
- **Posting rules in `gl`**: admins create journals/accounts then `gl.create_posting_rule`. Zero amounts in templates drop, so one stock rule can cover receipt, issue, adjust, and manufacture.
- **Templates as helpers**: `stock_rule_templates`, `bill_rule_templates`, `invoice_rule_templates` return example line maps (placeholder codes like `1400` Inventory, `2000` GRNI, `2100` AP, `1100` AR, `4000` revenue); they do not write rules themselves.
- **COGS / inventory** come from stock posted on delivery; sales invoice template only AR/revenue.

## Functions, commands, events

**Handlers**

| Function | Role |
|---|---|
| `forward` | Listener; skips transfers, else calls `gl.on_posting_event` |
| `stock_rule_templates` | Example templates for stock posted/voided |
| `bill_rule_templates` | Example template for purchase bill approved |
| `invoice_rule_templates` | Example template for sales invoice approved |

**Listen**

| Event | Notes |
|---|---|
| `erp_inventory.stock_posted` | Skip when `kind` is `transfer` |
| `erp_inventory.stock_voided` | Same skip |
| `erp_procurement.purchase_bill_approved` | Clear GRNI / credit AP (via rule) |
| `erp_selling.sales_invoice_approved` | Debit AR / credit revenue (via rule) |

**Emit**: none.

## Roles

None. Resulting entries are posted under kernel event delivery into `gl`.

## Boundaries

- Does not own stock quantities, bills, or invoices; only reacts to their approved/posted events.
- Does not emit accounting events back into ERP.
- Placeholder account codes in templates must be mapped before `create_posting_rule`.
- Requires `gl` posting rules; without them, forwarded deliveries fail and retry.
