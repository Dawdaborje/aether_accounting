# General ledger review (gl 0.1.0)

Written 2026-10-08. An outside review of the `gl` plugin was checked claim by claim against the code in
`plugins/accounting/gl/src` and the event delivery in the kernel (`facets/core/src/plugin_events.rs`). Nothing
has been fixed yet: this document records what is true, what is not, and the order of the fixes.

## Verdict in one table

| # | Claim | Verdict | Where |
|---|---|---|---|
| 1 | Posting is gapless and race-safe: entry and both counter increments in one transaction, unique `seq`, `number`, `idem_key`, four tries | True by design. No concurrent-posting test has been run, so it is not proven | `post.rs` `post`, models `gl_entry` |
| 2 | Idempotency key uses the rule's version, not the document's | **True, worst finding** | `post.rs` `source_key`, `from_event` |
| 3 | Event entries are dated by processing day | **True** | `post.rs` `from_event` (`today_date()`) |
| 4 | An event with no active rule is lost | **True** (returns `posted:false` as a success) | `from_event` |
| 5 | Failed events have no recovery at all | **Partly wrong.** Delivery is a scheduler job with `max_attempts` (default 3) and a job that fails for good stays visible. There is no dead-letter handler | `plugin_events.rs`, BREAKS item 43 |
| 6 | The hash covers less than it appears to | **True** | `rules.rs` `canonical` |
| 7 | `verify` misses trailing deletions | **True** | `reports.rs` `verify` |
| 8 | Renaming a journal makes `verify` report tampering | **True** (the code is read from the live journal) | `reports.rs` `verify`, entry stores only the journal id |
| 9 | `gl_admin` can write periods, ledger, journals, accounts directly | **True** | `rules/gl_period.json`, `gl_ledger.json`, `gl_journal.json`, `gl_account.json` |
| 10 | An admin can pre-empt a real event with a forged payload | **True** | `from_event` allows `gl_admin` |
| 11 | Allocation can over-allocate under concurrency; `auto_allocate` can half-finish | **True** | `alloc.rs` |
| 12 | Reports truncate silently at 1,000 lines | **True** | `reports.rs` `general_ledger`, `alloc.rs` `open_items`, `open_of` |
| 13 | The repair job only covers the latest 200 entries | **True** | `post.rs` `index_recent_entries` |
| 14 | A 45-line cap limits payroll to about 40 employees | **Real cap, not hit today.** The payroll event carries component totals, not a line per employee | `rules.rs` `MAX_LINES` |
| 15 | `restrict` with `seq < 0` may not mean "never allowed" | **Wrong.** `restrict` allows the operation only when the condition holds; `seq < 0` never holds, so entries cannot be changed or deleted | `rules/gl_entry.json` |
| 16 | Journal counters never reset by year; fiscal years are calendar months only; scale 2 is hard-coded | **True** | `numbered`, `setup.rs` `fiscal_year`, `Decimal::zero(2)` |

## The findings in detail

### 2. Idempotency key (fix first)
The key is `event:source_id:v<rule version>`. The event payload's own document version is never read.
- Publishing rule v2 and redelivering an old event posts it a second time (new key).
- A document that really changed (an expense report approved again with other amounts) under the same rule
  version gets the old entry back and the change is dropped.

Fix: key on a document version taken from the payload (a `version_path` on the rule, like `source_path`), and
store the rule version on the entry as information only. An event without the version path is refused when the
rule asks for one.

### 3. Entry date
`from_event` posts on the day it runs. A payroll run approved on 31 March and processed on 1 April lands in
April, or fails if March is closed. Fix: a `date_path` on the rule (for payroll, `period_end`); no date in the
payload means the rule must say which date to use, and "today" has to be chosen on purpose.

### 4 and 5. Lost and failed events
- No active rule: the call succeeds, the job completes, nobody sees it. Fix: refuse (fail the job) so it stays
  visible and is retried once the rule exists, or write a `gl_unposted` row an accountant can see.
- Failures retry up to the subscription's `max_attempts`, then stay as failed jobs. Needs the kernel's
  job-failed hook (BREAKS item 43) for anything better than a listing of failed jobs.

### 6 to 8. What the hash protects
`canonical` covers `seq`, `number`, `date`, journal code, currency and each line's account, debit, credit,
party and tax code. It leaves out `reverses`, `reason`, entry and line memos, `source_*`, `idem_key`, `period`,
`posted_by`, `posted_at`. So the link from a reversal to the entry it corrects is not tamper-evident. Fields
are joined with `|` and newline without escaping, so a party containing them is ambiguous. `verify` rebuilds
with the journal's current code, so renaming a journal reports every entry of it as changed. `verify` also
never compares the last entry with `gl_ledger.seq`, so deleting entries from the end is not noticed.

Fix (new `HASH_VERSION`, old entries still checked with the old rule): hash `reverses`, `source_kind`,
`source_id`, `source_version`, `reason` and the journal code stored on the entry itself; length-prefix or
escape every field; `verify` compares its last sequence with the ledger and reports the difference. Keep an
anchor (the last hash, exported or emitted as an event) outside the database as the real answer to deletion.

### 9. Direct admin writes
`gl_admin` has `create` and `write` on `gl_period`, `gl_ledger`, `gl_journal` and `gl_account`. Through the
generic record API an admin can set a closed period back to open, change `gl_ledger.seq` or `gl_journal.next`
(breaking gaplessness), change a used account's kind, parent, `is_group` or `is_control`, or rename a journal
code. Closing is final only through `close_period`.

Fix: grant `create` only; make `gl_period.status`, `gl_ledger.seq`, `gl_journal.next` and `code`, and the
structure of an account that has lines, writable only by the plugin's own handlers (a `restrict` that never
matches for generic writes, or field rules). Needs a check of what a handler can still write once the rule is
in place, since handlers run as the caller.

### 10. Forged events
`from_event` refuses members without `gl_admin`, but an admin can call `on_posting_event` with a payload
naming a real document. The genuine event then matches the forged key. Fix: once the key includes the
document version this is narrower; also refuse direct calls from members entirely and let only the event
path (the `gl_hrms` forwarder, which runs as the kernel) call it, using the call trail when the kernel exposes
it (BREAKS item 50).

### 11. Allocation
`allocate` reads the open amount of both lines and then creates a row, with no lock or transaction around the
read. Two concurrent calls can both pass the check. `auto_allocate` writes in chunks of 40, so a failure
leaves a partial match. Fix: write the allocation and a per-line guard (a counter on the line that must equal
what was read) in one transaction, and make `auto_allocate` report how far it got and resume.

### 12 and 13. Truncation and repair
`general_ledger` takes at most 1,000 lines and computes `closing` from the opening plus those lines;
`open_items` and `open_of` also stop at 1,000 without saying so. A busy control account will eventually show
wrong figures. `index_recent_entries` repairs only the latest 200 entries, so an older entry whose line rows
were never written stays out of the trial balance (`verify` counts these, nothing repairs them).

Fix: page (cursor on date, seq, line number) or return `truncated: true` and refuse a closing balance that
is not complete; take the closing balance from an aggregate like the opening; make the repair job walk from
the oldest unindexed entry.

### 14. The 45-line cap
A transaction holds 50 writes; an entry takes one and its counters two, so `MAX_LINES` is 45. The payroll
event posts component totals, so it is not reached. A payable line per employee (a party on every line) would
reach it at about 40 people; that would need the lines stored apart from the entry or the entry split.

### 15. Rules semantics (answer to the open question)
`restrict` entries are extra conditions a record must meet for the named operations; they are ANDed with the
grants. `{"seq": {"lt": 0}}` is never true, so `write` and `delete` on `gl_entry` are always refused. The
`tamper_test` feature of `gl` exists to prove it on a running kernel.

### 16. Smaller points
- Journal numbers are `<prefix>-<year>-<next>` with `next` never reset: the second year starts at a high
  number. Fix: a counter per journal and year.
- `create_fiscal_year` builds calendar-year months only. Fix: a start month, and 13-period or 4-4-5 years.
- Scale 2 is fixed in `Decimal::zero(2)` and the models, which rules out a base currency like JPY or KWD.
  Fix: the ledger holds its scale and every amount uses it.

## Order of work
1. Idempotency on the document version, and the date from the event (2, 3).
2. Rules: remove direct admin writes (9) and refuse direct event calls (10).
3. Hash version 2 with the stored journal code and escaping, `verify` against the ledger (6 to 8).
4. Atomic allocation (11).
5. Truncation flags or paging, and the repair walk (12, 13).
6. No-rule events visible (4).
7. Year counters, fiscal year shapes, ledger scale (16).

Each step gets its checks in the `gl` scenario: a changed document under the same rule version, a redelivered
event after a new rule version, an entry posted after midnight, a renamed journal, a deleted last entry, two
allocations at once, and a 1,200-line account.
