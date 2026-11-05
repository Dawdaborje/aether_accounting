# End-to-end checks of gl_payment (and the gl functions it uses) against a running kernel.
#
# Needs: the server running (`aether -c project/test/aether.toml --serve`), an organization with gl, gl_payment and
# gl_erp installed, and four logins in it: an organization admin (owner) and acct_clerk (gl_payment.pay_clerk),
# acct_a and acct_b (gl.gl_accountant). See README.md next to this file.
#
# Usage: python3 gl_payment_e2e.py [--base http://localhost:7890] [--org acct_test] [-v]
# Every run uses fresh party ids, so it can run again on the same organization.
import argparse, json, secrets, sys, urllib.error, urllib.request
from decimal import Decimal

ap = argparse.ArgumentParser()
ap.add_argument("--base", default="http://localhost:7890")
ap.add_argument("--year", type=int, default=2026)
ap.add_argument("--org", default="acct_test")
ap.add_argument("--password", default="Test-pass-123")
ap.add_argument("-v", action="store_true")
args = ap.parse_args()
RUN = secrets.token_hex(3)


class Session:
    """A logged-in person: cookies kept by hand, the CSRF token is double-submitted."""

    def __init__(self, username):
        self.cookies = {"aether_csrf": secrets.token_hex(16)}
        self.name = username
        self.post("/api/auth/login", {"username": username, "password": args.password})
        self.post("/api/auth/org", {"org": args.org})

    def post(self, path, body):
        request = urllib.request.Request(args.base + path, data=json.dumps(body).encode(), method="POST")
        request.add_header("content-type", "application/json")
        request.add_header("origin", args.base)
        request.add_header("x-csrf-token", self.cookies["aether_csrf"])
        request.add_header("cookie", "; ".join(f"{k}={v}" for k, v in self.cookies.items()))
        try:
            response = urllib.request.urlopen(request)
            status, raw, headers = response.status, response.read(), response.headers
        except urllib.error.HTTPError as error:
            status, raw, headers = error.code, error.read(), error.headers
        for cookie in headers.get_all("set-cookie") or []:
            name, _, rest = cookie.partition("=")
            self.cookies[name.strip()] = rest.split(";")[0]
        try:
            data = json.loads(raw) if raw else None
        except ValueError:
            data = {"_raw": raw.decode(errors="replace")}
        if status >= 400:
            raise Failed(status, data)
        return data

    def call(self, plugin, function, body=None):
        answer = self.post(f"/api/plugins/{plugin}/{function}", body or {})
        if args.v:
            print(f"    {self.name} {plugin}.{function} -> {json.dumps(answer)[:300]}")
        return answer


class Failed(Exception):
    def __init__(self, status, data):
        super().__init__(f"{status}: {json.dumps(data)[:400]}")
        self.status, self.data = status, data


results = []


def check(name, condition, detail=""):
    results.append((name, bool(condition)))
    print(("  ok    " if condition else "  FAIL  ") + name + ("" if condition else f"  [{detail}]"))


def refused(name, action, words):
    try:
        action()
    except Failed as failure:
        message = json.dumps(failure.data)
        check(name, any(w in message for w in words), message[:300])
        return
    check(name, False, "was allowed")


def d(value):
    return Decimal(str(value)) if value is not None else Decimal(0)


# ---------------------------------------------------------------------------------------------------------------
# Setup: a ledger, a small chart, journals, a fiscal year, a term and a bank. Each step is skipped when done.

Y = args.year
owner, clerk, acc_a, acc_b = (Session(u) for u in ("acct_owner", "acct_clerk", "acct_a", "acct_b"))

print("setup")
try:
    owner.call("gl", "get_ledger")
except Failed:
    owner.call("gl", "setup_ledger", {"name": "Test ledger", "base_currency": "USD"})
codes = {a["code"] for a in owner.call("gl", "list_accounts")}
for code, name, kind, control in [
    ("1000", "Bank", "asset", False), ("1010", "Old bank", "asset", False), ("1100", "Accounts receivable", "asset", True),
    ("1400", "Inventory", "asset", False), ("1450", "Work in progress", "asset", False),
    ("2000", "Goods received not invoiced", "liability", False), ("2100", "Accounts payable", "liability", True),
    ("3000", "Opening balances", "equity", False), ("4000", "Sales revenue", "income", False),
    ("5000", "Cost of goods sold", "expense", False), ("5100", "Inventory adjustment", "expense", False), ("5200", "Purchase price variance", "expense", False),
]:
    if code not in codes:
        owner.call("gl", "create_account", {"code": code, "name": name, "kind": kind, "is_control": control})
journals = {j["code"] for j in owner.call("gl", "list_journals")}
for code, name, kind, prefix in [("BANK", "Bank", "bank", "BNK"), ("OPEN", "Opening", "general", "OPN"), ("SAL", "Sales", "general", "SAL"),
                                 ("PUR", "Purchases", "purchase", "PUR"), ("STK", "Stock", "general", "STK")]:
    if code not in journals:
        owner.call("gl", "create_journal", {"code": code, "name": name, "kind": kind, "prefix": prefix})
owner.call("gl", "create_fiscal_year", {"year": Y})
terms = {t["name"]: t["id"] for t in owner.call("gl_payment", "list_terms")}
if "Net 30" not in terms:
    terms["Net 30"] = owner.call("gl_payment", "create_term", {"name": "Net 30", "net_days": 30})["id"]
methods = {m["name"]: m["id"] for m in owner.call("gl_payment", "list_methods")}
if "Main bank" not in methods:
    methods["Main bank"] = owner.call("gl_payment", "create_method", {"name": "Main bank", "kind": "bank", "account": "1000", "journal": "BANK"})["id"]
settings = owner.call("gl_payment", "setup_payments", {"receivable_account": "1100", "payable_account": "2100", "receivable_term": terms["Net 30"], "payable_term": terms["Net 30"]})
check("settings saved with four eyes on outgoing payments", settings["four_eyes_out"] is True)
refused("a clerk cannot change the settings", lambda: clerk.call("gl_payment", "setup_payments", {"receivable_account": "1100", "payable_account": "2100"}), ["gl.gl_admin"])
refused("a control account cannot be a payment method", lambda: owner.call("gl_payment", "create_method", {"name": f"bad {RUN}", "account": "1100", "journal": "BANK"}), ["control"])
BANK = methods["Main bank"]


def opening(side, party, number, total, date):
    """An invoice or bill from before the ledger: posted against opening balances, then registered."""
    receivable = side == "receivable"
    owner.call("gl", "post_sourced_entry", {
        "journal": "OPEN", "date": date, "memo": f"Opening {number}",
        "lines": [{"account": "1100" if receivable else "3000", "debit": total, "party": party if receivable else None},
                  {"account": "3000" if receivable else "2100", "credit": total, "party": None if receivable else party}],
        "source": {"kind": "opening", "id": number},
    })
    return owner.call("gl_payment", "register_document", {
        "side": side, "source_kind": "doc:opening", "source_id": number, "number": number, "party": party, "date": date, "total": total,
    })


def open_of(document_id, side, party):
    rows = owner.call("gl_payment", "outstanding_documents", {"side": side, "party": party})["documents"]
    return sum((d(r["open"]) for r in rows if r["document"] == document_id), Decimal(0))


def ledger_open(account, party):
    """Open debits minus open credits on a control account for a party, by the ledger's own report."""
    items = owner.call("gl", "open_items_report", {"account": account, "party": party})["items"]
    return sum((d(i["open"]) if i["side"] == "debit" else -d(i["open"]) for i in items), Decimal(0))


# ---------------------------------------------------------------------------------------------------------------
print("1. an invoice is registered, linked to its ledger line, and due by the party's term")
C = f"party:cust{RUN}"
inv1 = opening("receivable", C, f"INV-{RUN}-1", "1000.00", f"{Y}-09-01")
check("linked to the ledger line at once", bool(inv1.get("gl_line")), inv1)
check("due 30 days later", inv1["due_date"] == f"{Y}-10-01", inv1["due_date"])
again = owner.call("gl_payment", "register_document", {"side": "receivable", "source_kind": "doc:opening", "source_id": f"INV-{RUN}-1", "number": f"INV-{RUN}-1", "party": C, "date": f"{Y}-09-01", "total": "1000.00"})
check("registering it again changes nothing", again["id"] == inv1["id"])
check("open 1000", open_of(inv1["id"], "receivable", C) == Decimal("1000.00"))
refused("a clerk cannot register documents", lambda: clerk.call("gl_payment", "register_document", {"side": "receivable", "source_kind": "x", "source_id": "y", "party": C, "date": f"{Y}-09-01", "total": "1"}), ["bridge"])
refused("a document in another currency is refused", lambda: owner.call("gl_payment", "register_document", {"side": "receivable", "source_kind": "doc:opening", "source_id": f"EUR-{RUN}", "party": C, "date": f"{Y}-09-01", "total": "5", "currency": "EUR"}), ["EUR"])

print("2. a part payment: drafted by the clerk, confirmed by an accountant")
p1 = clerk.call("gl_payment", "create_payment", {"direction": "in", "party": C, "method": BANK, "date": f"{Y}-10-02", "amount": "600.00", "reference": "wire 1", "applies": [{"document": inv1["id"], "amount": "600.00"}]})["payment"]
refused("the clerk cannot confirm", lambda: clerk.call("gl_payment", "confirm_payment", {"id": p1["id"]}), ["gl.gl_accountant"])
done = acc_a.call("gl_payment", "confirm_payment", {"id": p1["id"]})["payment"]
check("confirmed and numbered from number_sequence", done["state"] == "confirmed" and done["number"].startswith(f"PAY-IN-{Y}-"), done)
check("invoice open 400", open_of(inv1["id"], "receivable", C) == Decimal("400.00"))
again = acc_a.call("gl_payment", "confirm_payment", {"id": p1["id"]})["payment"]
check("confirming again changes nothing", again["gl_entry"] == done["gl_entry"] and open_of(inv1["id"], "receivable", C) == Decimal("400.00"))

print("3. paying more than is open: refused when applied, kept as an advance when not")
p2 = clerk.call("gl_payment", "create_payment", {"direction": "in", "party": C, "method": BANK, "date": f"{Y}-10-03", "amount": "500.00", "applies": [{"document": inv1["id"], "amount": "500.00"}]})["payment"]
refused("applying 500 to an invoice with 400 open is refused", lambda: acc_a.call("gl_payment", "confirm_payment", {"id": p2["id"]}), ["only 400.00 open"])
clerk.call("gl_payment", "apply_to", {"payment": p2["id"], "document": inv1["id"], "amount": "400.00"})
acc_a.call("gl_payment", "confirm_payment", {"id": p2["id"]})
doc = [x for x in owner.call("gl_payment", "outstanding_documents", {"side": "receivable", "party": C})["documents"] if x["document"] == inv1["id"]]
check("the invoice is settled", not doc, doc)
check("the ledger holds 100 as an advance (credit) for the party", ledger_open("1100", C) == Decimal("-100.00"), ledger_open("1100", C))

print("4. the advance goes to the next invoice")
inv2 = opening("receivable", C, f"INV-{RUN}-2", "300.00", f"{Y}-07-01")
acc_a.call("gl_payment", "apply_advance", {"payment": p2["id"], "document": inv2["id"]})
check("second invoice open 200", open_of(inv2["id"], "receivable", C) == Decimal("200.00"))
check("ledger and documents agree (200 open, no advance)", ledger_open("1100", C) == Decimal("200.00"), ledger_open("1100", C))
aging = owner.call("gl_payment", "aging_report", {"side": "receivable", "party": C, "as_of": f"{Y}-10-14"})
check("aging: due 07-31, 75 days late, in 61-90", d(aging["total"]["61_90"]) == Decimal("200.00"), aging["total"])

print("5. cancelling a payment undoes its matches and reverses its entry")
refused("a void of a paid invoice is refused", lambda: owner.call("gl_payment", "void_document", {"source_kind": "doc:opening", "source_id": f"INV-{RUN}-1"}), ["cancel that payment first"])
cancelled = acc_b.call("gl_payment", "cancel_payment", {"id": p2["id"], "reason": "bounced"})["payment"]
check("cancelled with a reversal entry", cancelled["state"] == "cancelled" and bool(cancelled.get("reverse_entry")), cancelled)
check("first invoice open 400 again", open_of(inv1["id"], "receivable", C) == Decimal("400.00"))
check("second invoice open 300 again", open_of(inv2["id"], "receivable", C) == Decimal("300.00"))
check("ledger and documents agree (700 open)", ledger_open("1100", C) == Decimal("700.00"), ledger_open("1100", C))

print("6. money out needs a second person")
V = f"party:supp{RUN}"
bill = opening("payable", V, f"BILL-{RUN}-1", "800.00", f"{Y}-09-15")
p3 = acc_a.call("gl_payment", "create_payment", {"direction": "out", "party": V, "method": BANK, "date": f"{Y}-10-05", "amount": "800.00", "applies": [{"document": bill["id"], "amount": "800.00"}]})["payment"]
refused("the drafter cannot confirm their own outgoing payment", lambda: acc_a.call("gl_payment", "confirm_payment", {"id": p3["id"]}), ["someone other"])
out = acc_b.call("gl_payment", "confirm_payment", {"id": p3["id"]})["payment"]
check("a second accountant confirms it", out["state"] == "confirmed" and out["number"].startswith(f"PAY-OUT-{Y}-"), out)
check("the bill is settled and the ledger agrees", open_of(bill["id"], "payable", V) == 0 and ledger_open("2100", V) == 0, ledger_open("2100", V))
refused("a receivable invoice cannot be applied to a payment out", lambda: acc_a.call("gl_payment", "create_payment", {"direction": "out", "party": C, "method": BANK, "date": f"{Y}-10-05", "amount": "1.00", "applies": [{"document": inv1["id"], "amount": "1.00"}]}), ["not payable"])

print("7. failures leave nothing half done")
p4 = clerk.call("gl_payment", "create_payment", {"direction": "in", "party": C, "method": BANK, "date": f"{Y - 5}-06-01", "amount": "10.00"})["payment"]
refused("a payment dated outside any period is refused", lambda: acc_a.call("gl_payment", "confirm_payment", {"id": p4["id"]}), ["no period"])
still = owner.call("gl_payment", "get_payment", {"id": p4["id"]})["payment"]
check("it stays a draft, numbered, with no entry", still["state"] == "draft" and bool(still.get("number")) and not still.get("gl_entry"), still)
voided = acc_a.call("gl_payment", "cancel_payment", {"id": p4["id"], "reason": "wrong date"})["payment"]
check("cancelling it voids the number", voided["state"] == "cancelled")
audit = owner.call("number_sequence", "audit_gaps", {"code": "pay_in"})
check("the number sequence has no unexplained gap", not audit.get("missing"), audit)
old = owner.call("gl_payment", "create_method", {"name": f"Old bank {RUN}", "kind": "bank", "account": "1010", "journal": "BANK"})["id"]
owner.call("gl_payment", "deactivate_method", {"id": old})
refused("an inactive method is refused", lambda: clerk.call("gl_payment", "create_payment", {"direction": "in", "party": C, "method": old, "date": f"{Y}-10-05", "amount": "1.00"}), ["not in use"])
refused("a duplicate method name is refused in words", lambda: owner.call("gl_payment", "create_method", {"name": f"Old bank {RUN}", "account": "1010", "journal": "BANK"}), ["name may be taken"])

print("8. statement and register")
st = owner.call("gl_payment", "party_statement", {"party": C, "side": "receivable"})
check("statement closes at what is open (700)", d(st["closing"]) == Decimal("700.00"), st["closing"])
reg = owner.call("gl_payment", "payment_register", {"party": C})
check("the register lists the party's 3 payments", len(reg) == 3, len(reg))

print("9. a real ERP invoice reaches the ledger and gl_payment through gl_erp")
import time
rules = {r["event"] for r in owner.call("gl", "list_posting_rules") if r.get("is_active")}
for templates, journal in [("stock_rule_templates", "STK"), ("invoice_rule_templates", "SAL"), ("bill_rule_templates", "PUR")]:
    for event, t in owner.call("gl_erp", templates)["events"].items():
        if event not in rules:
            owner.call("gl", "create_posting_rule", {"event": event, "journal": journal, "source_path": t["source_path"], "version_path": t.get("version_path"),
                                                     "date_path": t["date_path"], "currency_path": t.get("currency_path"), "memo": t.get("memo"), "lines": t["lines"]})
units = owner.call("uom", "list_units", {})["rows"]
if not units:
    owner.call("uom", "seed_units", {})
    units = owner.call("uom", "list_units", {})["rows"]
unit = next((u for u in units if u.get("code") in ("unit", "units", "pcs", "ea", "each")), units[0])
companies = owner.call("company", "list_companies", {})["rows"]
company = next((x for x in companies if x.get("name") == "Acct Test Co"), None) or owner.call("company", "create_company", {"name": "Acct Test Co", "currency_code": "USD"})
customer = owner.call("party", "create_organization", {"name": f"Customer {RUN}"})
category = owner.call("erp_item", "create_category", {"code": f"cat{RUN}", "name": f"Goods {RUN}", "valuation_method": "average"})
item = owner.call("erp_item", "create_item", {"code": f"item{RUN}", "name": f"Widget {RUN}", "uom": unit["id"], "category": category["id"]})
owner.call("erp_item", "set_item_company", {"item": item["id"], "company": company["id"], "valuation_method": "average"})
house = owner.call("erp_inventory", "create_warehouse", {"code": f"wh{RUN}", "name": f"Main {RUN}", "company": company["id"]})
receipt = owner.call("erp_inventory", "draft_receipt", {"company": company["id"], "posting_date": f"{Y}-10-01", "currency": "USD"})
owner.call("erp_inventory", "add_line", {"move": receipt["id"], "item": item["id"], "qty": "10", "warehouse": house["id"], "unit_cost": "20.00"})
owner.call("erp_inventory", "post_move", {"id": receipt["id"]})
so = owner.call("erp_selling", "create_so", {"company": company["id"], "customer": customer["id"], "currency": "USD", "order_date": f"{Y}-10-02", "warehouse": house["id"]})
line = owner.call("erp_selling", "add_so_line", {"so": so["id"], "item": item["id"], "qty": "4", "unit_price": "50.00"})
owner.call("erp_selling", "confirm_so", {"id": so["id"]})
owner.call("erp_selling", "deliver_so", {"so": so["id"], "delivery_date": f"{Y}-10-03", "lines": [{"so_line": line["id"], "qty": "4"}]})
invoice = owner.call("erp_selling", "create_invoice", {"so": so["id"], "invoice_date": f"{Y}-10-04", "lines": [{"so_line": line["id"], "qty": "4"}]})
owner.call("erp_selling", "approve_invoice", {"id": invoice["id"]})
# The event is delivered in the background: wait for the bridge to register the invoice.
found = None
for _ in range(60):
    rows = owner.call("gl_payment", "outstanding_documents", {"side": "receivable", "party": customer["id"]})["documents"]
    found = next((r for r in rows if r["number"] == invoice["number"]), None)
    if found and found["linked"]:
        break
    time.sleep(2)
check("the approved invoice is registered by gl_erp", found is not None, invoice.get("number"))
check("and linked to its receivable line, with the party on it (template fix)", bool(found and found["linked"]), found)
check("open 200 (4 x 50), due 30 days after the invoice", bool(found) and d(found["open"]) == Decimal("200.00") and found["due_date"] == f"{Y}-11-03", found)
if found:
    pay = clerk.call("gl_payment", "create_payment", {"direction": "in", "party": customer["id"], "method": BANK, "date": f"{Y}-10-10", "amount": "200.00", "applies": [{"document": found["document"], "amount": "200.00"}]})["payment"]
    acc_b.call("gl_payment", "confirm_payment", {"id": pay["id"]})
    check("paying it settles it in gl_payment and in the ledger", open_of(found["document"], "receivable", customer["id"]) == 0 and ledger_open("1100", customer["id"]) == 0, ledger_open("1100", customer["id"]))
tb = owner.call("gl", "trial_balance_report")
check("the trial balance still balances", d(tb.get("total_debit", tb.get("debit", 0))) == d(tb.get("total_credit", tb.get("credit", 0))), {k: v for k, v in tb.items() if k != "rows"})

passed = sum(1 for _, ok in results if ok)
print(f"\n{passed}/{len(results)} checks passed (run {RUN})")
sys.exit(0 if passed == len(results) else 1)
