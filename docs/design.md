# Accounting: design from the sources

Studied: ERPNext `accounts/general_ledger.py`, `doctype/{gl_entry,account,period_closing_voucher,payment_reconciliation,
exchange_rate_revaluation}`, `utils.py`, `report/{trial_balance,general_ledger}`; Odoo 19 `account/models/{account_move,
account_move_line,account_partial_reconcile,account_journal,sequence_mixin,company}.py`, `template_generic_coa.py`.
Scope decided with the user: HRMS first (done), accounting as its own track; `hr_expense` and payroll only emit events and
accept reimbursement events.

| Source | Behaviour | Weakness | Aether `gl` |
|---|---|---|---|
| ERPNext GL Entry | One row per line, SUM(debit) - SUM(credit); cancel marks rows `is_cancelled` in place (an opt-in immutable mode appends mirrored rows) | Floats with a 0.5 / 5-minor-unit tolerance and auto round-off rows hide imbalance; the exchange-gain Journal Entry is exempt from the balance check; three overlapping close mechanisms; a second ledger (Payment Ledger) kept in step by code | Entries and lines are written once, in one transaction, never updated or deleted (a rule forbids it); a correction is a reversing entry that points at the original; balance must be exact; one place checks it; periods are records |
| Odoo account.move | One table for invoices, payments, statements and tax; `balance` computed from `amount_currency`; stored mutable residuals; opt-in hash per journal and sequence prefix; five lock dates | Money depends on ORM ordering; a posted move can go back to draft when the journal is not hashed; names parsed by regex for the sequence | Source documents stay in their plugins; the ledger turns events into entries by versioned posting rules, keyed by (source, id, version) so a repeat is harmless; a real counter per journal; one hash chain over the whole ledger, always on, with a verification report; open amounts derived from allocation rows |
| Both | Reconciliation rewrites or cancels vouchers (ERPNext) or creates side-effect moves (Odoo) | History changes while matching | Allocation rows are written once; undoing one writes a negative row; the open amount of a line is its amount less its allocations |

Decisions for v1:
* **Accounts**: a tree (asset, liability, equity, income, expense), groups cannot be posted to, a control account needs a
  party on every line, an account in use can be deactivated but never deleted.
* **Journals**: general, purchase, payroll, bank; each with a prefix and a counter, numbers are `PREFIX-YEAR-00001`,
  gapless (the counter moves in the same transaction as the entry; a collision retries).
* **Periods**: records with open / closed. Posting needs an open period; closing is final; a correction goes into the
  open period with a reference to the original.
* **Entries**: at least two lines, each a debit or a credit above zero, exact balance in one currency, at most 45 lines.
  Single (base) currency in v1: a posting in another currency is refused rather than mislabelled.
* **Hash chain**: each entry carries `seq`, `prev_hash` and `hash` (SHA-256 over the previous hash and the entry's
  canonical text), version 1; `verify_chain` replays it.
* **Posting rules**: data, versioned, per event; a rule line names an account, a side, an amount path in the payload and
  optionally a party path, and may repeat per item of a list (`each`) with an account map. Lines with the same account and
  party are merged; zero amounts are dropped.
* **Allocations** between a debit line and a credit line on the same control account and party; `auto_allocate` matches
  oldest first; an allocation is undone by a negative row.
* **Reports** over the lines: trial balance (with group roll-up), general ledger with running balance, journal listing,
  open items per party, chain verification.
* Deferred: multi-currency lines and exchange differences, tax tables, dimensions beyond a free map, cash basis,
  bank statement matching, budgets, revaluation, reversing entries in bulk, year-end close entries.
