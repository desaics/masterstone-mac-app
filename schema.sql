-- =============================================================================
-- Masterstone CRM — SQLite schema
-- =============================================================================
-- Generated for Session 2 of the Mac app migration.
-- Strategy (per architecture doc):
--   - Each main table has indexed columns for fields that are filtered, sorted,
--     or aggregated, plus a `raw_data` JSON column for nested arrays and rarely
--     queried fields.
--   - Multi-user readiness: `created_by`, `modified_by`, `modified_at` columns
--     are pre-populated now (defaulted to 'chintan') so that adding a second
--     user later is mechanical, not a re-architecture.
--   - Primary keys are TEXT (existing IDs preserved where present; generated
--     for contracts which have no native ID).
-- =============================================================================

PRAGMA foreign_keys = ON;
-- Note: WAL mode intentionally NOT set here. Migration produces a single
-- self-contained .db file. The Tauri runtime (Session 3+) can switch to WAL
-- on first open for better crash recovery and read concurrency.

-- -----------------------------------------------------------------------------
-- 1. clients — flattened from ms_client_master_v1 (a name-keyed dict)
-- -----------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS clients (
    company_name        TEXT PRIMARY KEY,
    short_name          TEXT,
    industry            TEXT,
    tier                TEXT,
    status              TEXT,
    gstin               TEXT,
    pan                 TEXT,
    state_code          TEXT,
    is_reseller         INTEGER NOT NULL DEFAULT 0,    -- boolean as 0/1
    reseller_name       TEXT,
    vendor_code         TEXT,
    -- nested arrays (contacts, billingAddresses, shippingAddresses, gstEntries, customTerms) live here
    raw_data            TEXT NOT NULL,                 -- JSON
    created_at          TEXT,
    updated_at          TEXT,
    created_by          TEXT NOT NULL DEFAULT 'chintan',
    modified_by         TEXT NOT NULL DEFAULT 'chintan',
    modified_at         TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX IF NOT EXISTS idx_clients_status      ON clients(status);
CREATE INDEX IF NOT EXISTS idx_clients_industry    ON clients(industry);
CREATE INDEX IF NOT EXISTS idx_clients_gstin       ON clients(gstin);
CREATE INDEX IF NOT EXISTS idx_clients_state_code  ON clients(state_code);

-- -----------------------------------------------------------------------------
-- 2. resellers — flattened from ms_reseller_master_v1 (also name-keyed)
-- -----------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS resellers (
    company_name        TEXT PRIMARY KEY,
    short_name          TEXT,
    status              TEXT,
    gstin               TEXT,
    pan                 TEXT,
    state_code          TEXT,
    needs_completion    INTEGER NOT NULL DEFAULT 0,
    raw_data            TEXT NOT NULL,
    created_at          TEXT,
    updated_at          TEXT,
    created_by          TEXT NOT NULL DEFAULT 'chintan',
    modified_by         TEXT NOT NULL DEFAULT 'chintan',
    modified_at         TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX IF NOT EXISTS idx_resellers_status    ON resellers(status);

-- -----------------------------------------------------------------------------
-- 3. oems — flattened from ms_oem_master_v1 (name → product list dict)
-- -----------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS oems (
    oem_name            TEXT PRIMARY KEY,
    raw_data            TEXT NOT NULL DEFAULT '{}',    -- room for OEM metadata later
    created_at          TEXT,
    modified_at         TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

-- -----------------------------------------------------------------------------
-- 4. products — exploded from oems[name] = [product_name, ...]
-- -----------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS products (
    oem_name            TEXT NOT NULL,
    product_name        TEXT NOT NULL,
    PRIMARY KEY (oem_name, product_name),
    FOREIGN KEY (oem_name) REFERENCES oems(oem_name) ON DELETE CASCADE ON UPDATE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_products_oem ON products(oem_name);

-- -----------------------------------------------------------------------------
-- 5. contracts — flattened from ms_pro_v210 (array, no native IDs)
--     Generated IDs: con_legacy_001 .. con_legacy_NNN
--     legacy_idx preserved so existing references like invoice.linkedContractIdx
--     can be resolved during the cutover.
-- -----------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS contracts (
    id                          TEXT PRIMARY KEY,
    legacy_idx                  INTEGER UNIQUE,        -- original position in the array
    client_name                 TEXT NOT NULL,
    product                     TEXT,
    internal_po                 TEXT,
    client_po                   TEXT,
    vendor_code                 TEXT,
    start_date                  TEXT,
    end_date                    TEXT,
    term_months                 INTEGER,
    cost_usd                    REAL,
    cost_currency               TEXT,
    sell_inr                    REAL,
    base_fx                     REAL,
    is_reseller                 INTEGER NOT NULL DEFAULT 0,
    reseller_name               TEXT,
    commercial_model            TEXT,
    sourced_via_reseller_key    TEXT,
    renewal_status              TEXT,
    -- billingYears, clientPOs, renewalActivities and the rest of the 38 fields
    raw_data                    TEXT NOT NULL,
    created_by                  TEXT NOT NULL DEFAULT 'chintan',
    modified_by                 TEXT NOT NULL DEFAULT 'chintan',
    modified_at                 TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX IF NOT EXISTS idx_contracts_client_name             ON contracts(client_name);
CREATE INDEX IF NOT EXISTS idx_contracts_renewal_status          ON contracts(renewal_status);
CREATE INDEX IF NOT EXISTS idx_contracts_end_date                ON contracts(end_date);
CREATE INDEX IF NOT EXISTS idx_contracts_commercial_model        ON contracts(commercial_model);
CREATE INDEX IF NOT EXISTS idx_contracts_sourced_via_reseller    ON contracts(sourced_via_reseller_key);

-- -----------------------------------------------------------------------------
-- 6. prospects — from ms_prospects_v1 (array; native id field exists)
-- -----------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS prospects (
    id                          TEXT PRIMARY KEY,
    company                     TEXT NOT NULL,
    opp_name                    TEXT,
    stage                       TEXT,
    priority                    TEXT,
    source                      TEXT,
    owner                       TEXT,
    acv                         REAL,
    licenses                    INTEGER,
    term_months                 INTEGER,
    currency                    TEXT,
    commercial_model            TEXT,
    sourced_via_reseller_key    TEXT,
    close_date                  TEXT,
    actual_close_date           TEXT,
    start_date                  TEXT,
    archived                    INTEGER NOT NULL DEFAULT 0,
    -- activities, stageHistory, oems[], proposalIds[], tags[], wonDetails
    raw_data                    TEXT NOT NULL,
    created_at                  TEXT,
    updated_at                  TEXT,
    created_by                  TEXT NOT NULL DEFAULT 'chintan',
    modified_by                 TEXT NOT NULL DEFAULT 'chintan',
    modified_at                 TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX IF NOT EXISTS idx_prospects_stage              ON prospects(stage);
CREATE INDEX IF NOT EXISTS idx_prospects_priority           ON prospects(priority);
CREATE INDEX IF NOT EXISTS idx_prospects_company            ON prospects(company);
CREATE INDEX IF NOT EXISTS idx_prospects_archived           ON prospects(archived);

-- -----------------------------------------------------------------------------
-- 7. proposals — from ms_proposals_v1 (currently empty; structure expected
--    to mirror invoices/contracts with line items + terms)
-- -----------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS proposals (
    id                          TEXT PRIMARY KEY,
    proposal_number             TEXT,
    client_name                 TEXT,
    proposal_date               TEXT,
    valid_until                 TEXT,
    status                      TEXT,
    grand_total                 REAL,
    commercial_model            TEXT,
    sourced_via_reseller_key    TEXT,
    raw_data                    TEXT NOT NULL,
    created_by                  TEXT NOT NULL DEFAULT 'chintan',
    modified_by                 TEXT NOT NULL DEFAULT 'chintan',
    modified_at                 TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX IF NOT EXISTS idx_proposals_status         ON proposals(status);
CREATE INDEX IF NOT EXISTS idx_proposals_client_name    ON proposals(client_name);

-- -----------------------------------------------------------------------------
-- 8. purchase_orders — from ms_purchase_orders_v1 (currently empty)
-- -----------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS purchase_orders (
    id                          TEXT PRIMARY KEY,
    po_number                   TEXT,
    vendor_name                 TEXT,
    po_date                     TEXT,
    status                      TEXT,
    grand_total                 REAL,
    raw_data                    TEXT NOT NULL,
    created_by                  TEXT NOT NULL DEFAULT 'chintan',
    modified_by                 TEXT NOT NULL DEFAULT 'chintan',
    modified_at                 TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX IF NOT EXISTS idx_purchase_orders_status   ON purchase_orders(status);
CREATE INDEX IF NOT EXISTS idx_purchase_orders_vendor   ON purchase_orders(vendor_name);

-- -----------------------------------------------------------------------------
-- 9. invoices — from ms_invoices_v1 (sales invoices / AR)
-- -----------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS invoices (
    id                          TEXT PRIMARY KEY,
    invoice_number              TEXT,
    invoice_date                TEXT,
    due_date                    TEXT,
    client_name                 TEXT,
    status                      TEXT,
    gst_mode                    TEXT,                  -- 'inter' / 'intra'
    place_of_supply_code        TEXT,
    grand_total                 REAL,
    gross_total                 REAL,
    discount_total              REAL,
    gst_total                   REAL,
    cgst                        REAL,
    sgst                        REAL,
    igst                        REAL,
    amount_paid                 REAL NOT NULL DEFAULT 0,
    amount_outstanding          REAL,
    paid_at                     TEXT,
    cancelled_at                TEXT,
    linked_contract_idx         TEXT,                  -- legacy reference (string-encoded int)
    linked_cycle_year_idx       TEXT,
    linked_proposal_id          TEXT,
    commercial_model            TEXT,
    sourced_via_reseller_key    TEXT,
    -- lineItems, payments, statusHistory, irn data, qrCodeImageDataUrl, etc.
    raw_data                    TEXT NOT NULL,
    created_at                  TEXT,
    updated_at                  TEXT,
    issued_at                   TEXT,
    created_by                  TEXT NOT NULL DEFAULT 'chintan',
    modified_by                 TEXT NOT NULL DEFAULT 'chintan',
    modified_at                 TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX IF NOT EXISTS idx_invoices_status              ON invoices(status);
CREATE INDEX IF NOT EXISTS idx_invoices_client_name         ON invoices(client_name);
CREATE INDEX IF NOT EXISTS idx_invoices_invoice_date        ON invoices(invoice_date);
CREATE INDEX IF NOT EXISTS idx_invoices_due_date            ON invoices(due_date);
CREATE INDEX IF NOT EXISTS idx_invoices_linked_contract_idx ON invoices(linked_contract_idx);
CREATE INDEX IF NOT EXISTS idx_invoices_commercial_model    ON invoices(commercial_model);

-- -----------------------------------------------------------------------------
-- 10. attachments — index only; actual file bytes live on disk per Decision 2A
--     (OneDrive subfolder). Currently empty in the source backup.
-- -----------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS attachments (
    id                      TEXT PRIMARY KEY,
    related_entity_type     TEXT,                      -- 'invoice','contract','proposal',etc.
    related_entity_id       TEXT,
    filename                TEXT,
    mime_type               TEXT,
    size_bytes              INTEGER,
    file_path               TEXT,                      -- relative path under OneDrive root
    fallback_url            TEXT,                      -- original OneDrive URL, if any
    uploaded_at             TEXT,
    raw_data                TEXT NOT NULL DEFAULT '{}',
    created_by              TEXT NOT NULL DEFAULT 'chintan',
    modified_at             TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX IF NOT EXISTS idx_attachments_related ON attachments(related_entity_type, related_entity_id);

-- -----------------------------------------------------------------------------
-- 11. commission_accruals — from ms_commissions_v1.accruals
-- -----------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS commission_accruals (
    id                          TEXT PRIMARY KEY,
    reseller_key                TEXT,
    invoice_id                  TEXT,
    invoice_number              TEXT,
    client_name                 TEXT,
    commercial_model            TEXT,
    commission_base_inr         REAL,
    commission_pct              REAL,
    commission_amount_inr       REAL,
    accrual_date                TEXT,
    invoice_paid_date           TEXT,
    source_contract_idx         INTEGER,                -- preserved from legacy
    backfilled                  INTEGER NOT NULL DEFAULT 0,
    raw_data                    TEXT NOT NULL,
    created_at                  TEXT,
    created_by                  TEXT NOT NULL DEFAULT 'chintan',
    modified_at                 TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX IF NOT EXISTS idx_accruals_reseller     ON commission_accruals(reseller_key);
CREATE INDEX IF NOT EXISTS idx_accruals_invoice      ON commission_accruals(invoice_id);
CREATE INDEX IF NOT EXISTS idx_accruals_accrual_date ON commission_accruals(accrual_date);

-- -----------------------------------------------------------------------------
-- 12. commission_payouts — from ms_commissions_v1.payouts (currently empty)
-- -----------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS commission_payouts (
    id                      TEXT PRIMARY KEY,
    reseller_key            TEXT,
    payout_date             TEXT,
    amount_inr              REAL,
    accrual_ids             TEXT,                       -- JSON array of accrual IDs covered
    raw_data                TEXT NOT NULL DEFAULT '{}',
    created_at              TEXT,
    created_by              TEXT NOT NULL DEFAULT 'chintan',
    modified_at             TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX IF NOT EXISTS idx_payouts_reseller  ON commission_payouts(reseller_key);

-- -----------------------------------------------------------------------------
-- 13. company_profile — single-row table (id always = 1)
-- -----------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS company_profile (
    id                          INTEGER PRIMARY KEY CHECK (id = 1),
    legal_name                  TEXT,
    trading_name                TEXT,
    gstin                       TEXT,
    state_code                  TEXT,
    pan                         TEXT,
    cin                         TEXT,
    -- Big base64 image strings — candidate for file extraction in Session 5/6
    logo_data_url               TEXT,
    letterhead_data_url         TEXT,
    signature_image_data_url    TEXT,
    letterhead_use_as_background INTEGER NOT NULL DEFAULT 0,
    -- Numbering config
    invoice_prefix              TEXT,
    proposal_prefix             TEXT,
    po_prefix                   TEXT,
    invoice_next_number         INTEGER,
    proposal_next_number        INTEGER,
    po_next_number              INTEGER,
    numbering_mode              TEXT,
    -- Banking
    bank_name                   TEXT,
    bank_branch                 TEXT,
    bank_account_holder         TEXT,
    bank_account_number         TEXT,
    bank_ifsc                   TEXT,
    -- Signatory
    signatory_name              TEXT,
    signatory_designation       TEXT,
    -- Misc + everything else (terms_templates, fy_serials, address, contact)
    raw_data                    TEXT NOT NULL,
    modified_at                 TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

-- -----------------------------------------------------------------------------
-- 14. app_settings — generic key-value store for view prefs, theme, schema_meta,
--     migration_state, etc. Avoids creating a separate tiny table per concern.
-- -----------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS app_settings (
    key                     TEXT PRIMARY KEY,
    value                   TEXT NOT NULL,
    modified_at             TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
