# General ledger (`gl`)

## Purpose

Double-entry ledger for one organization: chart of accounts, journals, fiscal periods, append-only entries with a hash chain, versioned posting rules for inbound events, and control-account allocations. Single currency (ledger `base_currency`). App route `/accounting`.

## Dependencies

None. Other domains reach this plugin through glue plugins that call `on_posting_event`.

## Models

| Model | Role |
|---|---|
| `gl_ledger` | Singleton: name, base currency, global `seq`, `indexed_seq` |
| `gl_account` | Chart: code, kind (asset/liability/equity/income/expense), parent, group/control/active |
| `gl_journal` | Named books with number `prefix` |
| `gl_period` | Fiscal slices (`open` / `closed`); closing is final and ordered |
| `gl_counter` | Per-journal per-year number counters (`key` = `{journal_id}:{year}`) |
| `gl_entry` | Sealed entry: number, seq, embedded lines, hash chain, source/idempotency, reversal link |
| `gl_line` | Indexed lines for queries (rebuilt by schedule if copy fails) |
| `gl_allocation` | Matches debit↔credit on control accounts; undo = negative row |
| `gl_posting_rule` | Versioned event → journal/lines template |

Access rules seal `gl_entry` against update/delete after post (`rules/gl_entry.json`).

## Design

- **Balance exactly**: no tolerance, no automatic round-off line; 2–45 lines; same-account same-side lines merge.
- **Append-only**: corrections are reversing entries (`reverses` + reason); never edit a posted entry. An entry with lines still matched (allocations not netted to zero by their undoings) cannot be reversed.
- **Gapless numbers**: `PREFIX-YEAR-NNNNN` from `gl_counter`; ledger `seq` is the global chain order.
- **Hash chain**: SHA-256 over canonical text (`hash_version` 2 length-prefixed; v1 still verifiable); genesis `prev_hash`; `verify_chain` replays it.
- **Periods**: post only into an open period covering the date; close earlier periods first.
- **Posting rules**: active rule for an event maps payload paths (`source_path`, `version_path`, `date_path`, amounts) into lines; zero amounts dropped; idempotency key by document (+ document version when set).
- **Allocations**: open amount = line amount − allocation rows; FIFO auto-match for a party.
-Deliberately strict on balance and immutability.

## Functions, commands, events

**Setup / admin** (`gl_admin`): `setup_ledger`, `create_account`, `deactivate_account`, `create_journal`, `create_fiscal_year`, `close_period`, `create_posting_rule`, `deactivate_posting_rule`.

**Posting** (`gl_accountant` or admin): `post_entry`, `reverse_entry`, `post_sourced_entry` (an entry for a document another plugin owns, such as a payment: idempotent per document and version, kind and key prefixed `doc:` so they can never take an event's key); kernel-only `on_posting_event` (members must use `post_entry`).

**Allocations**: `allocate_lines`, `undo_allocation`, `auto_allocate_party`, `open_items_report`; for plugins that keep documents (`gl_payment`), which cannot read the ledger's tables: `line_details` (open amount and allocations of up to 200 lines) and `source_lines` (the lines on one account of the non-reversed entries posted for a document).

**Reports** (`gl_reader`+): `get_ledger`, `list_accounts`, `list_journals`, `list_periods`, `list_posting_rules`, `get_entry`, `trial_balance_report`, `general_ledger_report`, `journal_listing_report`, `verify_chain`.

**Schedule**: `index_recent_entries` every 10m (fills missing `gl_line` rows, advances `indexed_seq`).

**Events**: none emitted. Inbound events arrive via `plugins::call("gl", "on_posting_event", …)` from bridge plugins.

## Roles

| Role | Scope |
|---|---|
| `gl_admin` | Ledger, chart, journals, periods, posting rules |
| `gl_accountant` | Post, reverse, allocate (admin may too) |
| `gl_reader` | Read models and reports |

## Boundaries

- Does not know HR, inventory, procurement, or sales; only posting-rule event names and payloads.
- No multi-currency posting yet; foreign currency is refused.
- Source plugins must not call `on_posting_event` as a member; glue plugins forward as the kernel.
- Groups are not postable; control accounts require a party on the line.
