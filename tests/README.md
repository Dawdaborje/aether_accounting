# Accounting end-to-end checks

`gl_payment_e2e.py` drives `gl`, `gl_payment` and `gl_erp` over HTTP as four people and checks that the
ledger and the payment documents always agree. Plain Python 3, no packages.

## One-time setup (the `project/test` project)

```sh
cd project/test
A="../../aether/target/debug/aether -c aether.toml"
$A --load-plugins ../../plugins/accounting/gl ../../plugins/accounting/gl_payment ../../plugins/accounting/gl_erp
$A --create-org "Accounting Test" --org-db-name acct_test --username acct_owner --email acct_owner@test.local --password Test-pass-123
$A --install-plugin gl_erp --org acct_test          # pulls gl, gl_payment, number_sequence, ERP, CRM, base
for u in acct_clerk acct_a acct_b; do
  $A --create-user --username $u --email $u@test.local --password Test-pass-123 --org-db-name acct_test
done
$A --grant-role gl_payment.pay_clerk --username acct_clerk --org acct_test
$A --grant-role gl.gl_accountant --username acct_a --org acct_test
$A --grant-role gl.gl_accountant --username acct_b --org acct_test
```

After changing a plugin: `make` in its folder, `--load-plugins` it again (development mode stores a new
revision), then `--upgrade-plugin <name> --org acct_test`.

## Running

With the server up (`aether -c project/test/aether.toml --serve`):

```sh
python3 plugins/accounting/tests/gl_payment_e2e.py          # -v prints every call
```

The script sets up the ledger, chart, journals, fiscal year, a term and a bank the first time, and uses fresh
parties on every run, so it can be run again and again. It exits 1 if any check fails.

| Section | What it checks |
|---|---|
| setup | settings, roles (a clerk cannot change them), a control account cannot be a payment method |
| 1 | an opening invoice is registered, linked to its ledger line, due by the term; registering again is a no-op; other currencies refused |
| 2 | clerk drafts, clerk cannot confirm, accountant confirms; numbered from `number_sequence`; confirming again changes nothing |
| 3 | applying more than is open is refused; an unapplied remainder stays in the ledger as an advance |
| 4 | the advance settles part of the next invoice; aging buckets |
| 5 | cancelling undoes matches and reverses the entry; voiding a paid invoice is refused |
| 6 | money out needs a second person; payable side end to end |
| 7 | a failed confirm leaves a numbered draft and no entry; cancelling voids the number (no gap); inactive methods refused |
| 8 | party statement and payment register |
| 9 | a real ERP sales invoice reaches `gl` and `gl_payment` through `gl_erp`, is paid, and the trial balance balances |

Section 9 needs the kernel to deliver plugin events (approving an invoice queues a `gl_erp.forward` job).
