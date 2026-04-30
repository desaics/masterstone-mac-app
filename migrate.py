#!/usr/bin/env python3
"""
Masterstone CRM — JSON backup → SQLite migrator.

Reads a Masterstone backup JSON file, creates a fresh SQLite database from
schema.sql, populates all 14 tables, and produces a per-table validation
report comparing source counts to destination counts.

Usage:
    python3 migrate.py <backup.json> <output.db> <schema.sql>

Exit codes:
    0  success, all counts match
    1  count mismatch (data was migrated but validation flagged a difference)
    2  fatal error (schema invalid, JSON invalid, etc.)
"""
from __future__ import annotations
import json
import sqlite3
import sys
from pathlib import Path
from typing import Any


# Localstorage keys that are plain strings (not JSON-encoded). Stored verbatim.
PLAIN_STRING_KEYS = {
    "ms_fy_basis_v1",
    "ms_invoices_view_v1",
    "ms_pipeline_view_v1",
    "ms_proposals_view_v1",
    "ms_theme",
}


def load_backup(path: Path) -> dict[str, Any]:
    """Load the backup envelope and parse each storage key's value."""
    with path.open() as f:
        envelope = json.load(f)

    storage = envelope["_storageKeys"]
    parsed: dict[str, Any] = {}
    for k, v in storage.items():
        if k in PLAIN_STRING_KEYS:
            parsed[k] = v
        else:
            try:
                parsed[k] = json.loads(v)
            except json.JSONDecodeError:
                # Treat as plain string if it isn't JSON
                parsed[k] = v
    return {"_envelope": envelope, **parsed}


def b(v: Any) -> int:
    """Boolean → 0/1 for SQLite."""
    return 1 if v else 0


def insert_clients(con: sqlite3.Connection, clients_dict: dict[str, dict]) -> int:
    """ms_client_master_v1 is keyed by company name → flatten."""
    rows = []
    for name, c in clients_dict.items():
        rows.append((
            name,
            c.get("shortName"),
            c.get("industry"),
            c.get("tier"),
            c.get("status"),
            c.get("gstin"),
            c.get("pan"),
            (c.get("gstEntries") or [{}])[0].get("stateCode") if c.get("gstEntries") else None,
            b(c.get("isReseller")),
            c.get("resellerName"),
            c.get("vendorCode"),
            json.dumps(c, ensure_ascii=False),
            c.get("createdAt"),
            c.get("updatedAt"),
        ))
    con.executemany(
        """INSERT INTO clients(
               company_name, short_name, industry, tier, status,
               gstin, pan, state_code, is_reseller, reseller_name,
               vendor_code, raw_data, created_at, updated_at)
           VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?)""",
        rows,
    )
    return len(rows)


def insert_resellers(con: sqlite3.Connection, resellers_dict: dict[str, dict]) -> int:
    rows = []
    for name, r in resellers_dict.items():
        gst_entries = r.get("gstEntries") or []
        rows.append((
            name,
            r.get("shortName"),
            r.get("status"),
            (gst_entries[0].get("gstin") if gst_entries else None),
            r.get("pan"),
            (gst_entries[0].get("stateCode") if gst_entries else None),
            b(r.get("needsCompletion")),
            json.dumps(r, ensure_ascii=False),
            r.get("createdAt"),
            r.get("updatedAt"),
        ))
    con.executemany(
        """INSERT INTO resellers(
               company_name, short_name, status, gstin, pan,
               state_code, needs_completion, raw_data, created_at, updated_at)
           VALUES(?,?,?,?,?,?,?,?,?,?)""",
        rows,
    )
    return len(rows)


def insert_oems_and_products(con: sqlite3.Connection, oems_dict: dict[str, list]) -> tuple[int, int]:
    oem_rows = []
    product_rows = []
    for oem_name, products in oems_dict.items():
        oem_rows.append((oem_name, json.dumps({"products": products}, ensure_ascii=False)))
        for p in products:
            product_rows.append((oem_name, p))
    con.executemany("INSERT INTO oems(oem_name, raw_data) VALUES(?,?)", oem_rows)
    con.executemany("INSERT INTO products(oem_name, product_name) VALUES(?,?)", product_rows)
    return len(oem_rows), len(product_rows)


def insert_contracts(con: sqlite3.Connection, contracts: list[dict]) -> int:
    """Contracts have no native id — generate stable con_legacy_NNN keys."""
    rows = []
    for idx, c in enumerate(contracts):
        cid = f"con_legacy_{idx:03d}"
        rows.append((
            cid,
            idx,
            c.get("client"),
            c.get("product"),
            c.get("internalPO"),
            c.get("clientPO"),
            c.get("vendorCode"),
            c.get("start"),
            c.get("endDate"),
            c.get("term"),
            c.get("costUSD"),
            c.get("costCurrency"),
            c.get("sellINR"),
            c.get("baseFX"),
            b(c.get("isReseller")),
            c.get("resellerName"),
            c.get("commercialModel"),
            c.get("sourcedViaResellerKey"),
            c.get("renewalStatus"),
            json.dumps(c, ensure_ascii=False),
        ))
    con.executemany(
        """INSERT INTO contracts(
               id, legacy_idx, client_name, product, internal_po, client_po,
               vendor_code, start_date, end_date, term_months, cost_usd, cost_currency,
               sell_inr, base_fx, is_reseller, reseller_name, commercial_model,
               sourced_via_reseller_key, renewal_status, raw_data)
           VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)""",
        rows,
    )
    return len(rows)


def insert_prospects(con: sqlite3.Connection, prospects: list[dict]) -> int:
    rows = []
    for p in prospects:
        rows.append((
            p["id"],
            p.get("company"),
            p.get("oppName"),
            p.get("stage"),
            p.get("priority"),
            p.get("source"),
            p.get("owner"),
            p.get("acv"),
            p.get("licenses"),
            p.get("term"),
            p.get("currency"),
            p.get("commercialModel"),
            p.get("sourcedViaResellerKey"),
            p.get("closeDate"),
            p.get("actualCloseDate"),
            p.get("startDate"),
            b(p.get("archived")),
            json.dumps(p, ensure_ascii=False),
            p.get("createdAt"),
            p.get("updatedAt"),
        ))
    con.executemany(
        """INSERT INTO prospects(
               id, company, opp_name, stage, priority, source, owner,
               acv, licenses, term_months, currency, commercial_model,
               sourced_via_reseller_key, close_date, actual_close_date, start_date,
               archived, raw_data, created_at, updated_at)
           VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)""",
        rows,
    )
    return len(rows)


def insert_proposals(con: sqlite3.Connection, proposals: list[dict]) -> int:
    rows = []
    for p in proposals:
        rows.append((
            p.get("id"),
            p.get("proposalNumber"),
            p.get("clientName"),
            p.get("proposalDate"),
            p.get("validUntil"),
            p.get("status"),
            p.get("grandTotal"),
            p.get("commercialModel"),
            p.get("sourcedViaResellerKey"),
            json.dumps(p, ensure_ascii=False),
        ))
    if rows:
        con.executemany(
            """INSERT INTO proposals(
                   id, proposal_number, client_name, proposal_date, valid_until,
                   status, grand_total, commercial_model, sourced_via_reseller_key, raw_data)
               VALUES(?,?,?,?,?,?,?,?,?,?)""",
            rows,
        )
    return len(rows)


def insert_purchase_orders(con: sqlite3.Connection, pos: list[dict]) -> int:
    rows = []
    for po in pos:
        rows.append((
            po.get("id"),
            po.get("poNumber"),
            po.get("vendorName"),
            po.get("poDate"),
            po.get("status"),
            po.get("grandTotal"),
            json.dumps(po, ensure_ascii=False),
        ))
    if rows:
        con.executemany(
            """INSERT INTO purchase_orders(
                   id, po_number, vendor_name, po_date, status, grand_total, raw_data)
               VALUES(?,?,?,?,?,?,?)""",
            rows,
        )
    return len(rows)


def insert_invoices(con: sqlite3.Connection, invoices: list[dict]) -> int:
    rows = []
    for i in invoices:
        rows.append((
            i["id"],
            i.get("invoiceNumber"),
            i.get("invoiceDate"),
            i.get("dueDate"),
            i.get("clientName"),
            i.get("status"),
            i.get("gstMode"),
            i.get("placeOfSupplyCode"),
            i.get("grandTotal"),
            i.get("grossTotal"),
            i.get("discountTotal"),
            i.get("gstTotal"),
            i.get("cgst"),
            i.get("sgst"),
            i.get("igst"),
            i.get("amountPaid", 0),
            i.get("amountOutstanding"),
            i.get("paidAt"),
            i.get("cancelledAt"),
            i.get("linkedContractIdx"),
            i.get("linkedCycleYearIdx"),
            i.get("linkedProposalId"),
            i.get("commercialModel"),
            i.get("sourcedViaResellerKey"),
            json.dumps(i, ensure_ascii=False),
            i.get("createdAt"),
            i.get("updatedAt"),
            i.get("issuedAt"),
        ))
    con.executemany(
        """INSERT INTO invoices(
               id, invoice_number, invoice_date, due_date, client_name, status,
               gst_mode, place_of_supply_code, grand_total, gross_total,
               discount_total, gst_total, cgst, sgst, igst, amount_paid,
               amount_outstanding, paid_at, cancelled_at, linked_contract_idx,
               linked_cycle_year_idx, linked_proposal_id, commercial_model,
               sourced_via_reseller_key, raw_data, created_at, updated_at, issued_at)
           VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)""",
        rows,
    )
    return len(rows)


def insert_attachments(con: sqlite3.Connection, attachments: list[dict]) -> int:
    rows = []
    for a in attachments:
        rows.append((
            a.get("id"),
            a.get("relatedEntityType"),
            a.get("relatedEntityId"),
            a.get("filename"),
            a.get("mimeType"),
            a.get("sizeBytes"),
            a.get("filePath"),
            a.get("fallbackUrl"),
            a.get("uploadedAt"),
            json.dumps(a, ensure_ascii=False),
        ))
    if rows:
        con.executemany(
            """INSERT INTO attachments(
                   id, related_entity_type, related_entity_id, filename, mime_type,
                   size_bytes, file_path, fallback_url, uploaded_at, raw_data)
               VALUES(?,?,?,?,?,?,?,?,?,?)""",
            rows,
        )
    return len(rows)


def insert_commissions(con: sqlite3.Connection, commissions: dict) -> tuple[int, int]:
    accruals = commissions.get("accruals", [])
    payouts = commissions.get("payouts", [])

    accrual_rows = [(
        a["id"],
        a.get("resellerKey"),
        a.get("invoiceId"),
        a.get("invoiceNumber"),
        a.get("clientName"),
        a.get("commercialModel"),
        a.get("commissionBaseINR"),
        a.get("commissionPct"),
        a.get("commissionAmountINR"),
        a.get("accrualDate"),
        a.get("invoicePaidDate"),
        a.get("sourceContractIdx"),
        b(a.get("backfilled")),
        json.dumps(a, ensure_ascii=False),
        a.get("createdAt"),
    ) for a in accruals]

    if accrual_rows:
        con.executemany(
            """INSERT INTO commission_accruals(
                   id, reseller_key, invoice_id, invoice_number, client_name,
                   commercial_model, commission_base_inr, commission_pct,
                   commission_amount_inr, accrual_date, invoice_paid_date,
                   source_contract_idx, backfilled, raw_data, created_at)
               VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)""",
            accrual_rows,
        )

    payout_rows = [(
        p["id"],
        p.get("resellerKey"),
        p.get("payoutDate"),
        p.get("amountInr"),
        json.dumps(p.get("accrualIds", []), ensure_ascii=False),
        json.dumps(p, ensure_ascii=False),
        p.get("createdAt"),
    ) for p in payouts]

    if payout_rows:
        con.executemany(
            """INSERT INTO commission_payouts(
                   id, reseller_key, payout_date, amount_inr, accrual_ids, raw_data, created_at)
               VALUES(?,?,?,?,?,?,?)""",
            payout_rows,
        )

    return len(accrual_rows), len(payout_rows)


def insert_company_profile(con: sqlite3.Connection, profile: dict) -> int:
    con.execute(
        """INSERT INTO company_profile(
               id, legal_name, trading_name, gstin, state_code, pan, cin,
               logo_data_url, letterhead_data_url, signature_image_data_url,
               letterhead_use_as_background,
               invoice_prefix, proposal_prefix, po_prefix,
               invoice_next_number, proposal_next_number, po_next_number,
               numbering_mode,
               bank_name, bank_branch, bank_account_holder, bank_account_number, bank_ifsc,
               signatory_name, signatory_designation,
               raw_data)
           VALUES(1,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)""",
        (
            profile.get("legalName"), profile.get("tradingName"), profile.get("gstin"),
            profile.get("stateCode"), profile.get("pan"), profile.get("cin"),
            profile.get("logoDataUrl"), profile.get("letterheadDataUrl"),
            profile.get("signatureImageDataUrl"),
            b(profile.get("letterheadUseAsBackground")),
            profile.get("invoicePrefix"), profile.get("proposalPrefix"), profile.get("poPrefix"),
            profile.get("invoiceNextNumber"), profile.get("proposalNextNumber"),
            profile.get("poNextNumber"), profile.get("numberingMode"),
            profile.get("bankName"), profile.get("bankBranch"),
            profile.get("bankAccountHolder"), profile.get("bankAccountNumber"),
            profile.get("bankIFSC"),
            profile.get("signatoryName"), profile.get("signatoryDesignation"),
            json.dumps(profile, ensure_ascii=False),
        ),
    )
    return 1


def insert_app_settings(con: sqlite3.Connection, parsed: dict) -> int:
    """View prefs, theme, schema_meta, migration_4A_state — all into key-value."""
    settings: list[tuple[str, str]] = []
    settings.append(("theme", parsed.get("ms_theme") or "light"))
    settings.append(("fy_basis", parsed.get("ms_fy_basis_v1") or "invoice"))
    settings.append(("invoices_view", parsed.get("ms_invoices_view_v1") or "list"))
    settings.append(("pipeline_view", parsed.get("ms_pipeline_view_v1") or "list"))
    settings.append(("proposals_view", parsed.get("ms_proposals_view_v1") or "list"))
    if "ms_schema_meta_v1" in parsed:
        settings.append(("schema_meta", json.dumps(parsed["ms_schema_meta_v1"], ensure_ascii=False)))
    if "ms_migration_4A_state_v1" in parsed:
        settings.append(("migration_4A_state", json.dumps(parsed["ms_migration_4A_state_v1"], ensure_ascii=False)))

    con.executemany("INSERT INTO app_settings(key, value) VALUES(?,?)", settings)
    return len(settings)


def main(backup_path: Path, db_path: Path, schema_path: Path) -> int:
    print(f"\n[1/4] Loading backup from {backup_path.name}...")
    parsed = load_backup(backup_path)
    print(f"      Backup version:  {parsed['_envelope'].get('_appBuild')}")
    print(f"      Exported at:     {parsed['_envelope'].get('_exportedAt')}")

    if db_path.exists():
        db_path.unlink()
        print(f"      Removed pre-existing {db_path.name}")

    print(f"\n[2/4] Creating fresh database at {db_path.name} from schema.sql...")
    con = sqlite3.connect(db_path)
    con.executescript(schema_path.read_text())
    print(f"      Schema applied. Tables created.")

    print(f"\n[3/4] Migrating data...")
    counts: dict[str, int] = {}

    counts["clients"]              = insert_clients(con, parsed.get("ms_client_master_v1", {}))
    counts["resellers"]            = insert_resellers(con, parsed.get("ms_reseller_master_v1", {}))
    oem_n, prod_n                  = insert_oems_and_products(con, parsed.get("ms_oem_master_v1", {}))
    counts["oems"]                 = oem_n
    counts["products"]             = prod_n
    counts["contracts"]            = insert_contracts(con, parsed.get("ms_pro_v210", []))
    counts["prospects"]            = insert_prospects(con, parsed.get("ms_prospects_v1", []))
    counts["proposals"]            = insert_proposals(con, parsed.get("ms_proposals_v1", []))
    counts["purchase_orders"]      = insert_purchase_orders(con, parsed.get("ms_purchase_orders_v1", []))
    counts["invoices"]             = insert_invoices(con, parsed.get("ms_invoices_v1", []))
    counts["attachments"]          = insert_attachments(con, parsed.get("ms_attachments_v1", []))
    acc_n, pay_n                   = insert_commissions(con, parsed.get("ms_commissions_v1", {}))
    counts["commission_accruals"]  = acc_n
    counts["commission_payouts"]   = pay_n
    counts["company_profile"]      = insert_company_profile(con, parsed.get("ms_company_profile_v1", {}))
    counts["app_settings"]         = insert_app_settings(con, parsed)

    con.commit()
    print("      Data committed.")

    print(f"\n[4/4] Validation — comparing source counts to destination counts:\n")
    expected_record_counts = parsed["_envelope"].get("_recordCounts", {})

    # Build a comparison table
    print(f"      {'Table':<24}{'Source (JSON)':>18}{'Destination (DB)':>20}{'Match':>10}")
    print(f"      {'-'*24}{'-'*18:>18}{'-'*20:>20}{'-'*10:>10}")

    source_lookups = {
        "clients":             ("ms_client_master_v1", lambda v: len(v)),
        "resellers":           ("ms_reseller_master_v1", lambda v: len(v)),
        "oems":                ("ms_oem_master_v1", lambda v: len(v)),
        "products":            ("ms_oem_master_v1", lambda v: sum(len(p) for p in v.values())),
        "contracts":           ("ms_pro_v210", lambda v: len(v)),
        "prospects":           ("ms_prospects_v1", lambda v: len(v)),
        "proposals":           ("ms_proposals_v1", lambda v: len(v)),
        "purchase_orders":     ("ms_purchase_orders_v1", lambda v: len(v) if isinstance(v, list) else 0),
        "invoices":            ("ms_invoices_v1", lambda v: len(v)),
        "attachments":         ("ms_attachments_v1", lambda v: len(v) if isinstance(v, list) else 0),
        "commission_accruals": ("ms_commissions_v1", lambda v: len(v.get("accruals", []))),
        "commission_payouts":  ("ms_commissions_v1", lambda v: len(v.get("payouts", []))),
        "company_profile":     ("ms_company_profile_v1", lambda v: 1 if v else 0),
    }

    all_match = True
    for table_name in counts:
        if table_name == "app_settings":
            # not a record-count comparison, just show DB count
            db_n = con.execute(f"SELECT COUNT(*) FROM {table_name}").fetchone()[0]
            print(f"      {table_name:<24}{'(meta)':>18}{db_n:>20}{'-':>10}")
            continue

        if table_name in source_lookups:
            src_key, src_fn = source_lookups[table_name]
            src_v = parsed.get(src_key)
            src_n = src_fn(src_v) if src_v else 0
        else:
            src_n = 0

        db_n = con.execute(f"SELECT COUNT(*) FROM {table_name}").fetchone()[0]
        ok = src_n == db_n
        if not ok:
            all_match = False
        marker = "✓" if ok else "✗ MISMATCH"
        print(f"      {table_name:<24}{src_n:>18}{db_n:>20}{marker:>10}")

    # Cross-check against the envelope's own _recordCounts as a third independent count
    print(f"\n      Cross-check against backup's own _recordCounts envelope:")
    envelope_map = {
        "clients": "clients",
        "contracts": "contracts",
        "prospects": "prospects",
        "proposals": "proposals",
        "purchaseOrders": "purchase_orders",
        "invoices": "invoices",
        "oems": "oems",
        "attachments": "attachments",
    }
    print(f"      {'Envelope key':<24}{'Envelope count':>18}{'DB count':>20}{'Match':>10}")
    print(f"      {'-'*24}{'-'*18:>18}{'-'*20:>20}{'-'*10:>10}")
    for ek, table in envelope_map.items():
        ev = expected_record_counts.get(ek, 0)
        dv = con.execute(f"SELECT COUNT(*) FROM {table}").fetchone()[0]
        ok = ev == dv
        if not ok:
            all_match = False
        marker = "✓" if ok else "✗ MISMATCH"
        print(f"      {ek:<24}{ev:>18}{dv:>20}{marker:>10}")

    db_size_kb = db_path.stat().st_size / 1024
    print(f"\n      Database file size: {db_size_kb:.1f} KB ({db_path.stat().st_size:,} bytes)")

    con.close()

    # Re-measure after close (without WAL we don't strictly need this, but it's
    # honest reporting in case the journal mode is ever flipped back to WAL).
    final_size = db_path.stat().st_size
    if final_size != int(db_size_kb * 1024):
        print(f"      Final size after connection close: {final_size/1024:.1f} KB ({final_size:,} bytes)")

    if all_match:
        print(f"\n[OK] All record counts match. Migration verified end-to-end.\n")
        return 0
    else:
        print(f"\n[FAIL] One or more record counts do not match. See ✗ markers above.\n")
        return 1


if __name__ == "__main__":
    if len(sys.argv) != 4:
        print(__doc__)
        sys.exit(2)
    try:
        rc = main(Path(sys.argv[1]), Path(sys.argv[2]), Path(sys.argv[3]))
        sys.exit(rc)
    except Exception as e:
        print(f"\n[FATAL] {type(e).__name__}: {e}")
        import traceback
        traceback.print_exc()
        sys.exit(2)
