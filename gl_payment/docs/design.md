# Payments and open documents (`gl_payment`)

Status: **built** (v0.1.0). End-to-end checks: [`../tests`](../../tests/README.md).

## Purpose

What customers owe the organization and what it owes suppliers (and employees), document by document, and the
money that settles it. Records incoming and outgoing payments, posts them to the ledger, matches them against the
documents they pay, and reports what is open and overdue. Single currency (the ledger's `base_currency`), like `gl`.

Scope: payments, payment terms, and aged receivable/payable reports.

## What already exists and is reused

`gl` already keeps open items, at the **line** level: a line on a control account carries a `party`, and
`gl_allocation` rows match a debit line against a credit line on the same account and party
(`allocate_lines`, `undo_allocation`, `auto_allocate_party`, `open_items_report`). The open amount of a line is
its amount minus its allocations.

This plugin does **not** keep a second record of what is owed. Settlement is always a `gl_allocation`; the open
amount of a document is the open amount of its control line in `gl`. What `gl` lacks, and this plugin adds:

| Missing in `gl` | Added here |
|---|---|
| Which document a control line belongs to (number, party, due date) | `pay_document` |
| When it is due | `pay_term`, `pay_party_term`, `due_date` on `pay_document` |
| A record of money moving, with a method and a bank account | `payment`, `pay_method` |
| Turning a payment into an entry and allocations in one step | `confirm_payment` |
| Aging, statements, what is overdue | Reports |

## Dependencies

| Dependency | Why |
|---|---|
| `gl` | Posts payment entries, reverses them, allocates and undoes allocations (`plugins::call`, as the same actor) |
| `number_sequence` (base) | Payment numbers (`next_number`, `void_number`, `audit_gaps`) |

Capabilities: `db::query`, `db::mutate`, `db::transaction`, `plugins::call`, `events::emit`.

Does **not** depend on ERP or HRMS. Bridges (`gl_erp`, `gl_hrms`) register documents here, the same way they hand
events to `gl` today.

## Models

| Model | Role |
|---|---|
| `pay_setting` | Singleton: receivable and payable control account codes, default term per side, payment journal code, sequence codes per direction, `four_eyes_out` (default on) |
| `pay_term` | Name, net days (`30`), optional end-of-month flag. Instalments are out of scope in this version |
| `pay_party_term` | Party → term, per side (`receivable` / `payable`) |
| `pay_method` | How money moves: name, kind (`bank` / `cash`), `gl` account code, journal code, active |
| `pay_document` | One per registered invoice, bill or expense report: side, `source_kind`, `source_id`, number, party, company, date, `due_date`, total, `gl_line` (control line, filled once found), state (`open` / `settled` / `void`) |
| `payment` | Direction (`in` / `out`), number, party, company, method, date, amount, reference, state (`draft` / `confirmed` / `cancelled`), `created_by`, `confirmed_by`, `gl_entry`, `gl_line`, `reverse_entry` |
| `payment_apply` | What a payment is meant to settle: payment, document, amount, `allocation` (the `gl_allocation` once written) |

`state` on `pay_document` is a cache refreshed after each allocation and by a schedule. The figures always come
from `gl`.

## Design

- **The ledger stays the truth.** A payment is a `gl_entry` (bank against the control account, with the party);
  settling is `gl_allocation`. Undoing a payment reverses the entry and undoes its allocations; nothing is
  edited or deleted.
- **Documents arrive through bridges.** `register_document` takes `{ side, source_kind, source_id, number, party,
  company, date, total, due_date? }`. It does nothing when the document is already registered with the same
  `source_kind`, `source_id` and version. When `due_date` is missing, it is the date plus the party's term, else
  the default term for that side.
- **Finding the control line.** A document's `gl` entry is the one whose `source_kind` / `source_id` match. Its
  line on the receivable or payable account for the document's party is stored in `gl_line`. Event delivery is
  asynchronous, so the line may not exist yet: the document stays `open` with no `gl_line`, and
  `link_documents` (schedule) fills it in later. A document without a line cannot be paid against.
- **Confirming a payment** (draft → confirmed):
  1. Number from `number_sequence.next_number` (codes `pay_in` / `pay_out`, scoped per company as
     `pay_in@<company>` with fallback to the unscoped sequence, gapless, yearly reset, e.g. `PAY-IN-2026-00001`).
     The number is stored on the payment at once, so a retried confirm reuses it instead of drawing another.
     A payment cancelled while still unposted calls `void_number` with the reason, so `audit_gaps` stays clean.
     `setup_payments` creates both sequences when they are missing.
  2. Post through `gl` with source `gl_payment.payment:{id}`, so a retry finds the same entry instead of posting
     twice. `in`: debit the method's account, credit receivable (party). `out`: debit payable (party), credit
     the method's account.
  3. For each `payment_apply`, allocate between the payment's control line and the document's control line,
     capped at what is open on both. Retrying allocates only what is still missing (target minus allocations
     already written between those two lines), so a confirm cut off halfway can be run again.
  4. Whatever is not applied stays open on the party as an advance. `auto_allocate_party` in `gl` or a later
     `apply_advance` matches it.
- **Cancelling** a confirmed payment undoes its allocations, then reverses its entry with a reason. The period
  of the reversal must be open (a `gl` rule).
- **Over-payment and part-payment** are both allowed. Paying more than the open total of the applied documents
  leaves an advance. Paying less leaves the documents part-open.
- **Write-off** of a small remainder is out of scope in this version (it needs a write-off account and an
  approval). Post it by hand with `gl.post_entry` and allocate.

## Changes made outside this plugin

| Where | Change | Why |
|---|---|---|
| `gl` | `post_sourced_entry` (role `gl_accountant`): an entry for a document with `source { kind, id, version }`, returning the existing entry for the same key; kind and key get a `doc:` prefix | `post_entry` has no source, so a retried confirm would post twice; `on_posting_event` refuses members by design |
| `gl` | `line_details` and `source_lines` (readers) | A plugin cannot read another plugin's tables; these are how this one sees what is open and finds a document's control line |
| `gl` | `reverse_entry` counts allocations net of their undoings | Before, an undone allocation still blocked the reversal, so a payment could never be cancelled |
| `gl` | `rules/gl_ledger.json` grants `gl_accountant` write (fields still `via:self`; `name` admin-only) | Every posting moves `gl_ledger.seq`, so an accountant could not post at all, by hand or through a payment; only kernel-delivered events worked |
| `gl_erp` | Invoice and bill templates put `"party": "customer"` / `"party": "supplier"` on the AR / AP line | AR and AP must be control accounts for open items, and `gl` refuses a control line without a party |
| `gl_erp` | Depends on `gl_payment` and calls `register_document` for `sales_invoice_approved` and `purchase_bill_approved`, after the ledger call (both safe to repeat) | Document number, party and date reach this plugin |
| `gl_hrms` | **Not done.** `hr_expense` records reimbursements itself and names employees by HR id, not by party, so expense reports need their own decision | — |

Calls to other plugins go through `common::call`, which keeps a called plugin's refusal as a message for the person:
the kernel returns it as a kernel error, which would otherwise reach them as "plugin invocation failed".

## Functions, commands, events

**Setup** (`gl_admin`): `setup_payments` (run again to change; creates the `pay_in` / `pay_out` sequences), `create_term`, `set_party_term`, `create_method`, `deactivate_method`; readers: `get_settings`, `list_terms`, `list_methods`.

**Documents** (bridges call these as the kernel; `gl_admin` may too, for documents open from before the ledger): `register_document`, `void_document`. An opening document is posted with `gl.post_sourced_entry` (kind `opening`) and registered with `source_kind` `doc:opening`.

**Payments** (`pay_clerk` or `gl_accountant` drafts; `gl_accountant` confirms and cancels; see Roles): `create_payment`, `update_draft`,
`apply_to`, `confirm_payment`, `cancel_payment`, `apply_advance`, `get_payment`. Confirming refuses an apply larger than what is open on the document.

**Reports** (`gl_reader`+): `outstanding_documents` (per party, side), `aging_report` (buckets by days past
due: current, 1–30, 31–60, 61–90, 90+), `party_statement` (documents, payments and running balance over a
date range), `payment_register`.

**Schedule**: `link_documents` every 10m (finds control lines for documents still missing one, refreshes cached
`state`).

**Events emitted**: `payment_confirmed`, `payment_cancelled`, `document_settled` (for example, so CRM can show a
customer as paid up, or `erp_procurement` can see a bill as paid).

## Roles

| Role | Scope |
|---|---|
| `pay_clerk` | Draft payments and choose what they settle; read documents |
| `gl_accountant` (from `gl`) | Confirm and cancel payments, apply advances |
| `gl_admin` (from `gl`) | Settings, terms, methods |

The confirming person needs `gl_accountant` because `gl` checks the role on the same actor during the call.

**Separation of duties.** Recording a payment and releasing money are different jobs, so a clerk (accounts
receivable / payable staff) can draft payments without being able to post to the ledger. For outgoing payments,
`four_eyes_out` (on by default) also refuses a confirm by the person who drafted it: money leaving the
organization is where fraud happens. Incoming payments may be confirmed by their drafter, since recording money
received carries far less risk. This is the same rule `hr_payroll` uses (the approver is not the person who made
the run). A small organization with one accountant can turn `four_eyes_out` off.

**Companies.** `gl` has no notion of a company: one ledger per organization, and no company on entries or lines.
So there is one receivable and one payable account per ledger. Documents and payments still record their
`company`, reports can be filtered by it, and payment numbers are scoped per company. Per-company control
accounts come when `gl` itself becomes company-aware, as a per-company override in `pay_setting`. That needs
no change to documents or payments.

## Boundaries

- Does not know invoices, bills or expense reports. It only knows registered documents.
- No foreign currency: a payment or document in another currency is refused, the same as in `gl`.
- No bank statements or reconciliation (that is `gl_bank`). No cheque printing or payment files (SEPA, NACHA).
- No credit notes yet, because ERP has none. When one exists, it registers as a document on the opposite side
  and is matched like a payment.
- No instalment terms, early-payment discounts or dunning letters in this version.

## Test plan

There are no plugin tests in the repo yet; these come first, run against a test org.

1. Invoice 1 000 approved → document registered, control line linked, open 1 000, aging "current".
2. Payment in 600 applied to it → entry balances, allocation 600, document open 400, state `open`.
3. Payment in 500 → 400 settles the invoice (`document_settled`), 100 left as an advance.
4. Confirm interrupted after posting → confirm again: one entry, allocations complete.
5. Cancel the second payment → allocations undone, entry reversed, document open 400 again.
6. Trial balance: receivable = sum of open documents − advances, for every step above.
7. Bill and payment out: the same steps on the payable side.
8. Payment in a foreign currency, on an inactive method, or into a closed period → refused, nothing written.
9. Outgoing payment confirmed by its drafter with `four_eyes_out` on → refused; by a second accountant → posted.
10. Two companies → separate number series, one receivable account, aging filtered by company adds up to the total.

## Decisions

1. `pay_clerk` exists. Outgoing payments need a second person by default (`four_eyes_out`).
2. Payment numbers come from `number_sequence` (base): one place to change numbering, with a gap audit for free.
3. One receivable and one payable account per ledger, with `company` kept on documents and payments. Per-company
   accounts wait for a company-aware `gl`.
