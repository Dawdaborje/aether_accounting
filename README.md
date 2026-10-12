# Accounting workspace

A double-entry general ledger plus small bridge plugins that turn HR and ERP events into ledger entries. Source documents stay in their own plugins; versioned posting rules in `gl` decide the accounts and amounts.

## Features

- Append-only entries that balance exactly, gapless journal numbers, and a SHA-256 hash chain
- Chart of accounts, journals, fiscal periods, control-account allocations
- Event posting through versioned rules (idempotent per document / version)
- Optional bridges: payroll & expenses (`gl_hrms`), stock / bills / invoices (`gl_erp`)
- Trial balance, account ledger, journal listing, chain verification
- Single base currency in this version

## Plugins

| Plugin | Role | Design |
|---|---|---|
| [`gl`](./gl) | Core ledger (models, posting, reports) | [docs/design.md](./gl/docs/design.md) |
| [`gl_hrms`](./gl_hrms) | Forwards HR payroll/expense events → `gl` | [docs/design.md](./gl_hrms/docs/design.md) |
| [`gl_erp`](./gl_erp) | Forwards ERP stock/bill/invoice events → `gl` | [docs/design.md](./gl_erp/docs/design.md) |

## How the pieces relate

```
hr_payroll / hr_expense ──events──► gl_hrms ──call──► gl.on_posting_event
erp_inventory / erp_procurement / erp_selling ──events──► gl_erp ──call──► gl
```

- **`gl`** never depends on HR or ERP. It only understands posting-rule event names and payloads.
- **`gl_hrms`** listens to `hr_payroll.payroll_run_approved` and `hr_expense.report_approved`.
- **`gl_erp`** listens to `erp_inventory.stock_posted` / `erp_inventory.stock_voided`, `erp_procurement.purchase_bill_approved`, and `erp_selling.sales_invoice_approved` (skips stock transfers).
- Upstream events are owned by the HRMS and ERP workspaces; bridges only forward them. Without an active `gl_posting_rule` for an event, delivery fails and is retried once a rule exists.

## Load order

Dependencies decide install/load order:

1. **`gl`** — no plugin dependencies; load first in this workspace.
2. Upstream domain plugins must already be available when a bridge is installed:
   - `gl_hrms` → `gl`, `hr_payroll`, `hr_expense`
   - `gl_erp` → `gl`, `erp_inventory`, `erp_procurement`, `erp_selling`
3. Bridges after `gl` and their upstream emitters.

Workspace declaration: `workspace.toml` (`gl`, `gl_hrms`, `gl_erp`).
