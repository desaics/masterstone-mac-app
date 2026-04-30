// Masterstone CRM — Tauri runtime entry.
//
// Session 4 scope: writes flow back to SQLite. The HTML CRM continues to use
// synchronous localStorage.setItem(); a JS storage adapter intercepts those
// calls and asynchronously fires the corresponding Rust command below.
//
// One write command per localStorage bucket. Each command:
//   1. Receives the entire JSON blob the CRM would have stored in localStorage.
//   2. Opens a transaction.
//   3. DELETEs all existing rows in the relevant table(s).
//   4. INSERTs new rows extracted from the JSON, populating both the typed
//      columns (for indexed queries later) and the raw_data column (the full
//      original JSON for that row).
//   5. Commits or rolls back atomically.
//
// Why DELETE-then-INSERT instead of UPSERT? The CRM's save model is "rewrite
// the whole bucket". Trying to diff-and-update would require tracking IDs and
// detecting deletions, doubling complexity. Wholesale rewrite is simpler,
// fast at this data volume (11 clients, 26 contracts, 2 invoices), and
// matches the existing semantics exactly.
//
// Concurrency: SQLite WAL mode is enabled on first open; combined with the
// JS-side write queue (debounced per bucket), there's never more than one
// in-flight writer to a given table.

use rusqlite::{params, Connection, OpenFlags};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

// ============================================================================
// Path resolution
// ============================================================================

fn db_path() -> Option<PathBuf> {
    let mut p = dirs::data_dir()?;
    p.push("com.masterstone.crm");
    p.push("masterstone.db");
    Some(p)
}

fn ensure_parent_exists(path: &PathBuf) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
}

fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

// ============================================================================
// State — a singleton Connection wrapped in Mutex for serialized writes.
// Read commands open their own short-lived read-only connection (cheap with
// WAL enabled); only writes go through the shared connection.
// ============================================================================

struct DbState {
    conn: Mutex<Option<Connection>>,
}

impl DbState {
    fn new() -> Self {
        Self { conn: Mutex::new(None) }
    }

    /// Open or reopen the connection in read-write mode and enable WAL.
    /// Called lazily before the first write, and explicitly when the
    /// first-launch wizard places a new file.
    fn ensure_open(&self) -> Result<(), String> {
        let mut guard = self.conn.lock().map_err(|e| format!("Mutex poisoned: {e}"))?;
        if guard.is_some() {
            return Ok(());
        }
        let path = db_path().ok_or("Could not resolve App Support directory")?;
        if !path.exists() {
            return Err("DB_NOT_FOUND".to_string());
        }
        let con = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE,
        ).map_err(|e| format!("Could not open database for writing: {e}"))?;
        // Enable WAL — better crash recovery and read concurrency. No-op if
        // already in WAL mode. Run as query rather than execute since it
        // returns the new mode.
        let _: String = con.query_row("PRAGMA journal_mode = WAL;", [], |r| r.get(0))
            .map_err(|e| format!("Could not enable WAL: {e}"))?;
        // Foreign keys per schema header.
        con.execute("PRAGMA foreign_keys = ON;", [])
            .map_err(|e| format!("Could not enable foreign keys: {e}"))?;
        *guard = Some(con);
        Ok(())
    }

    /// Force the next ensure_open to re-open the connection. Used after the
    /// first-launch wizard places a new file and we need to switch from "no
    /// connection" to "connected".
    fn reset(&self) {
        if let Ok(mut guard) = self.conn.lock() {
            *guard = None;
        }
    }
}

// ============================================================================
// Read path — unchanged from Session 3, just refactored to use a one-shot
// read-only connection (independent of the writer connection).
// ============================================================================

#[derive(Serialize)]
struct LoadAllResult {
    ok: bool,
    error_kind: Option<String>,
    error_detail: Option<String>,
    db_path: Option<String>,
    record_counts: BTreeMap<String, usize>,
    data: BTreeMap<String, String>,
}

impl LoadAllResult {
    fn err(kind: &str, detail: String, path: Option<PathBuf>) -> Self {
        Self {
            ok: false,
            error_kind: Some(kind.to_string()),
            error_detail: Some(detail),
            db_path: path.map(|p| p.to_string_lossy().to_string()),
            record_counts: BTreeMap::new(),
            data: BTreeMap::new(),
        }
    }
}

#[tauri::command]
fn storage_load_all() -> LoadAllResult {
    let path = match db_path() {
        Some(p) => p,
        None => return LoadAllResult::err(
            "PATH_RESOLVE_FAILED",
            "Could not resolve macOS Application Support directory.".to_string(),
            None,
        ),
    };

    if !path.exists() {
        ensure_parent_exists(&path);
        return LoadAllResult::err(
            "DB_NOT_FOUND",
            format!("Database file not found. Place masterstone.db at: {}", path.display()),
            Some(path),
        );
    }

    match load_all_inner(&path) {
        Ok(r) => r,
        Err(e) => LoadAllResult::err("DB_READ_ERROR", format!("{e}"), Some(path)),
    }
}

fn load_all_inner(path: &PathBuf) -> Result<LoadAllResult, rusqlite::Error> {
    // Read connection — separate from writer. With WAL, readers don't block
    // writers and vice versa.
    let con = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;

    let mut data: BTreeMap<String, String> = BTreeMap::new();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();

    // Contracts (array, ordered by legacy_idx)
    let contracts = collect_array(&con, "SELECT raw_data FROM contracts ORDER BY legacy_idx ASC")?;
    counts.insert("contracts".into(), contracts.len());
    data.insert("ms_pro_v210".into(), serde_json::Value::Array(contracts).to_string());

    // Clients (name-keyed dict)
    let clients_dict = collect_dict_by_column(&con, "SELECT company_name, raw_data FROM clients")?;
    counts.insert("clients".into(), clients_dict.len());
    data.insert("ms_client_master_v1".into(), serde_json::Value::Object(clients_dict).to_string());

    // Resellers (name-keyed dict)
    let resellers_dict = collect_dict_by_column(&con, "SELECT company_name, raw_data FROM resellers")?;
    counts.insert("resellers".into(), resellers_dict.len());
    data.insert("ms_reseller_master_v1".into(), serde_json::Value::Object(resellers_dict).to_string());

    // OEMs (name → product list dict)
    let mut oems_obj = serde_json::Map::new();
    {
        let mut stmt = con.prepare("SELECT oem_name FROM oems ORDER BY oem_name")?;
        let oem_rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        for oem_name_res in oem_rows {
            let oem_name = oem_name_res?;
            let mut prod_stmt = con.prepare(
                "SELECT product_name FROM products WHERE oem_name = ?1 ORDER BY product_name")?;
            let products_iter = prod_stmt.query_map([&oem_name], |r| r.get::<_, String>(0))?;
            let mut products: Vec<serde_json::Value> = Vec::new();
            for p in products_iter {
                products.push(serde_json::Value::String(p?));
            }
            oems_obj.insert(oem_name, serde_json::Value::Array(products));
        }
    }
    counts.insert("oems".into(), oems_obj.len());
    data.insert("ms_oem_master_v1".into(), serde_json::Value::Object(oems_obj).to_string());

    // Invoices (array)
    let invoices = collect_array(&con, "SELECT raw_data FROM invoices ORDER BY created_at, id")?;
    counts.insert("invoices".into(), invoices.len());
    data.insert("ms_invoices_v1".into(), serde_json::Value::Array(invoices).to_string());

    // Prospects (array)
    let prospects = collect_array(&con, "SELECT raw_data FROM prospects ORDER BY created_at, id")?;
    counts.insert("prospects".into(), prospects.len());
    data.insert("ms_prospects_v1".into(), serde_json::Value::Array(prospects).to_string());

    // Proposals (array)
    let proposals = collect_array(&con, "SELECT raw_data FROM proposals ORDER BY id")?;
    counts.insert("proposals".into(), proposals.len());
    data.insert("ms_proposals_v1".into(), serde_json::Value::Array(proposals).to_string());

    // Purchase Orders (array)
    let pos = collect_array(&con, "SELECT raw_data FROM purchase_orders ORDER BY id")?;
    counts.insert("purchase_orders".into(), pos.len());
    data.insert("ms_purchase_orders_v1".into(), serde_json::Value::Array(pos).to_string());

    // Commissions (object with accruals + payouts arrays)
    let accruals = collect_array(&con, "SELECT raw_data FROM commission_accruals ORDER BY accrual_date, id")?;
    let payouts = collect_array(&con, "SELECT raw_data FROM commission_payouts ORDER BY payout_date, id")?;
    counts.insert("commission_accruals".into(), accruals.len());
    counts.insert("commission_payouts".into(), payouts.len());
    let mut commissions_obj = serde_json::Map::new();
    commissions_obj.insert("accruals".into(), serde_json::Value::Array(accruals));
    commissions_obj.insert("payouts".into(), serde_json::Value::Array(payouts));
    data.insert("ms_commissions_v1".into(), serde_json::Value::Object(commissions_obj).to_string());

    // Company Profile (single dict)
    {
        let mut stmt = con.prepare("SELECT raw_data FROM company_profile WHERE id = 1")?;
        let mut rows = stmt.query([])?;
        if let Some(row) = rows.next()? {
            let raw: String = row.get(0)?;
            data.insert("ms_company_profile_v1".into(), raw);
            counts.insert("company_profile".into(), 1);
        } else {
            data.insert("ms_company_profile_v1".into(), "{}".to_string());
            counts.insert("company_profile".into(), 0);
        }
    }

    // Attachments (array)
    let attachments = collect_array(&con, "SELECT raw_data FROM attachments ORDER BY uploaded_at, id")?;
    counts.insert("attachments".into(), attachments.len());
    data.insert("ms_attachments_v1".into(), serde_json::Value::Array(attachments).to_string());

    // App settings → individual ms_* keys
    {
        let mut stmt = con.prepare("SELECT key, value FROM app_settings")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (k, v) = row?;
            let ls_key = match k.as_str() {
                "theme"               => "ms_theme",
                "fy_basis"            => "ms_fy_basis_v1",
                "invoices_view"       => "ms_invoices_view_v1",
                "pipeline_view"       => "ms_pipeline_view_v1",
                "proposals_view"      => "ms_proposals_view_v1",
                "schema_meta"         => "ms_schema_meta_v1",
                "migration_4A_state"  => "ms_migration_4A_state_v1",
                _ => continue,
            };
            data.insert(ls_key.into(), v);
        }
    }

    Ok(LoadAllResult {
        ok: true,
        error_kind: None,
        error_detail: None,
        db_path: Some(path.to_string_lossy().to_string()),
        record_counts: counts,
        data,
    })
}

fn collect_array(con: &Connection, sql: &str) -> Result<Vec<serde_json::Value>, rusqlite::Error> {
    let mut stmt = con.prepare(sql)?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut out: Vec<serde_json::Value> = Vec::new();
    for row in rows {
        let raw = row?;
        match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(v) => out.push(v),
            Err(_) => out.push(serde_json::Value::Null),
        }
    }
    Ok(out)
}

fn collect_dict_by_column(
    con: &Connection,
    sql: &str,
) -> Result<serde_json::Map<String, serde_json::Value>, rusqlite::Error> {
    let mut stmt = con.prepare(sql)?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut out = serde_json::Map::new();
    for row in rows {
        let (k, raw) = row?;
        let v = serde_json::from_str::<serde_json::Value>(&raw).unwrap_or(serde_json::Value::Null);
        out.insert(k, v);
    }
    Ok(out)
}

// ============================================================================
// Write path — one command per bucket.
//
// Each command takes the JSON string the CRM would have stored in localStorage
// (already serialized) and replaces the relevant table contents wholesale.
//
// Result envelope is uniform across all writes — the JS side checks .ok and
// surfaces .error to the status indicator.
// ============================================================================

#[derive(Serialize)]
struct SaveResult {
    ok: bool,
    error: Option<String>,
    rows_written: usize,
}

impl SaveResult {
    fn ok(n: usize) -> Self { Self { ok: true, error: None, rows_written: n } }
    fn err(e: String) -> Self { Self { ok: false, error: Some(e), rows_written: 0 } }
}

/// Helper: extract a string field from a JSON object, returning None if missing
/// or non-string. Used heavily by the write commands.
fn js_str<'a>(v: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str())
}
/// Helper: extract f64 — handles both number and numeric-string fields (the
/// CRM is loose about types; sometimes amounts come through as strings).
fn js_f64(v: &serde_json::Value, key: &str) -> Option<f64> {
    let val = v.get(key)?;
    if let Some(n) = val.as_f64() { return Some(n); }
    if let Some(s) = val.as_str() { return s.parse::<f64>().ok(); }
    None
}
/// Helper: extract i64 — same numeric leniency.
fn js_i64(v: &serde_json::Value, key: &str) -> Option<i64> {
    let val = v.get(key)?;
    if let Some(n) = val.as_i64() { return Some(n); }
    if let Some(s) = val.as_str() { return s.parse::<i64>().ok(); }
    None
}
/// Helper: extract a boolean as 0/1 integer.
fn js_bool_int(v: &serde_json::Value, key: &str) -> i64 {
    match v.get(key) {
        Some(serde_json::Value::Bool(b)) => if *b { 1 } else { 0 },
        Some(serde_json::Value::Number(n)) => if n.as_f64().unwrap_or(0.0) != 0.0 { 1 } else { 0 },
        _ => 0,
    }
}

/// Helper to acquire the writer connection inside a write command.
fn with_writer<F, R>(state: &DbState, f: F) -> Result<R, String>
where F: FnOnce(&mut Connection) -> rusqlite::Result<R>,
{
    state.ensure_open()?;
    let mut guard = state.conn.lock().map_err(|e| format!("Mutex poisoned: {e}"))?;
    let con = guard.as_mut().ok_or("DB connection unavailable")?;
    f(con).map_err(|e| format!("{e}"))
}

// ----------------------------------------------------------------------------
// Clients — name-keyed dict from ms_client_master_v1
// ----------------------------------------------------------------------------
#[tauri::command]
fn storage_save_clients(state: tauri::State<DbState>, payload: String) -> SaveResult {
    let parsed: serde_json::Value = match serde_json::from_str(&payload) {
        Ok(v) => v,
        Err(e) => return SaveResult::err(format!("Invalid JSON: {e}")),
    };
    let obj = match parsed.as_object() {
        Some(o) => o,
        None => return SaveResult::err("Expected object (name-keyed dict)".into()),
    };

    match with_writer(&state, |con| {
        let tx = con.transaction()?;
        tx.execute("DELETE FROM clients", [])?;
        let mut count = 0usize;
        for (name, value) in obj {
            let raw = value.to_string();
            tx.execute(
                "INSERT INTO clients (
                    company_name, short_name, industry, tier, status,
                    gstin, pan, state_code, is_reseller, reseller_name, vendor_code,
                    raw_data, created_at, updated_at, modified_at
                ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
                params![
                    name,
                    js_str(value, "shortName"),
                    js_str(value, "industry"),
                    js_str(value, "tier"),
                    js_str(value, "status"),
                    js_str(value, "gstin"),
                    js_str(value, "pan"),
                    js_str(value, "stateCode"),
                    js_bool_int(value, "isReseller"),
                    js_str(value, "resellerName"),
                    js_str(value, "vendorCode"),
                    raw,
                    js_str(value, "createdAt"),
                    js_str(value, "updatedAt"),
                    now_iso(),
                ],
            )?;
            count += 1;
        }
        tx.commit()?;
        Ok(count)
    }) {
        Ok(n) => SaveResult::ok(n),
        Err(e) => SaveResult::err(e),
    }
}

// ----------------------------------------------------------------------------
// Resellers — name-keyed dict from ms_reseller_master_v1
// ----------------------------------------------------------------------------
#[tauri::command]
fn storage_save_resellers(state: tauri::State<DbState>, payload: String) -> SaveResult {
    let parsed: serde_json::Value = match serde_json::from_str(&payload) {
        Ok(v) => v,
        Err(e) => return SaveResult::err(format!("Invalid JSON: {e}")),
    };
    let obj = match parsed.as_object() {
        Some(o) => o,
        None => return SaveResult::err("Expected object (name-keyed dict)".into()),
    };

    match with_writer(&state, |con| {
        let tx = con.transaction()?;
        tx.execute("DELETE FROM resellers", [])?;
        let mut count = 0usize;
        for (name, value) in obj {
            let raw = value.to_string();
            tx.execute(
                "INSERT INTO resellers (
                    company_name, short_name, status, gstin, pan, state_code,
                    needs_completion, raw_data, created_at, updated_at, modified_at
                ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                params![
                    name,
                    js_str(value, "shortName"),
                    js_str(value, "status"),
                    js_str(value, "gstin"),
                    js_str(value, "pan"),
                    js_str(value, "stateCode"),
                    js_bool_int(value, "needsCompletion"),
                    raw,
                    js_str(value, "createdAt"),
                    js_str(value, "updatedAt"),
                    now_iso(),
                ],
            )?;
            count += 1;
        }
        tx.commit()?;
        Ok(count)
    }) {
        Ok(n) => SaveResult::ok(n),
        Err(e) => SaveResult::err(e),
    }
}

// ----------------------------------------------------------------------------
// OEMs — name → product list dict from ms_oem_master_v1
// Writes both `oems` rows (one per OEM) and `products` rows (one per product).
// ----------------------------------------------------------------------------
#[tauri::command]
fn storage_save_oems(state: tauri::State<DbState>, payload: String) -> SaveResult {
    let parsed: serde_json::Value = match serde_json::from_str(&payload) {
        Ok(v) => v,
        Err(e) => return SaveResult::err(format!("Invalid JSON: {e}")),
    };
    let obj = match parsed.as_object() {
        Some(o) => o,
        None => return SaveResult::err("Expected object (oem→products dict)".into()),
    };

    match with_writer(&state, |con| {
        let tx = con.transaction()?;
        // Order matters: products has FK to oems, so delete products first.
        tx.execute("DELETE FROM products", [])?;
        tx.execute("DELETE FROM oems", [])?;
        let mut count = 0usize;
        for (oem_name, products_val) in obj {
            tx.execute(
                "INSERT INTO oems (oem_name, raw_data, modified_at) VALUES (?1, ?2, ?3)",
                params![oem_name, "{}", now_iso()],
            )?;
            count += 1;
            if let Some(arr) = products_val.as_array() {
                for product in arr {
                    if let Some(pname) = product.as_str() {
                        // Ignore duplicates within the same OEM (CRM may submit dupes)
                        let _ = tx.execute(
                            "INSERT OR IGNORE INTO products (oem_name, product_name) VALUES (?1, ?2)",
                            params![oem_name, pname],
                        );
                    }
                }
            }
        }
        tx.commit()?;
        Ok(count)
    }) {
        Ok(n) => SaveResult::ok(n),
        Err(e) => SaveResult::err(e),
    }
}

// ----------------------------------------------------------------------------
// Contracts — array from ms_pro_v210
// Generated IDs use legacy_idx position (con_legacy_NNN) to keep invoice
// linkedContractIdx references valid. New contracts (added after migration)
// get a UUID-style id; if the CRM hasn't generated one, we synthesize one.
// ----------------------------------------------------------------------------
#[tauri::command]
fn storage_save_contracts(state: tauri::State<DbState>, payload: String) -> SaveResult {
    let parsed: serde_json::Value = match serde_json::from_str(&payload) {
        Ok(v) => v,
        Err(e) => return SaveResult::err(format!("Invalid JSON: {e}")),
    };
    let arr = match parsed.as_array() {
        Some(a) => a,
        None => return SaveResult::err("Expected array of contracts".into()),
    };

    match with_writer(&state, |con| {
        let tx = con.transaction()?;
        tx.execute("DELETE FROM contracts", [])?;
        let mut count = 0usize;
        for (idx, value) in arr.iter().enumerate() {
            let raw = value.to_string();
            // Try existing id; otherwise synthesize from idx for legacy
            // continuity (matches Session 2 migration scheme).
            let id: String = match js_str(value, "id") {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => format!("con_legacy_{:03}", idx),
            };
            tx.execute(
                "INSERT INTO contracts (
                    id, legacy_idx, client_name, product, internal_po, client_po, vendor_code,
                    start_date, end_date, term_months,
                    cost_usd, cost_currency, sell_inr, base_fx,
                    is_reseller, reseller_name, commercial_model, sourced_via_reseller_key,
                    renewal_status, raw_data, modified_at
                ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21)",
                params![
                    id,
                    idx as i64,
                    js_str(value, "client").or(js_str(value, "clientName")).unwrap_or(""),
                    js_str(value, "product"),
                    js_str(value, "internalPO").or(js_str(value, "internal_po")),
                    js_str(value, "clientPO").or(js_str(value, "client_po")),
                    js_str(value, "vendorCode"),
                    js_str(value, "startDate").or(js_str(value, "start_date")),
                    js_str(value, "endDate").or(js_str(value, "end_date")),
                    js_i64(value, "termMonths").or(js_i64(value, "term_months")),
                    js_f64(value, "costUSD").or(js_f64(value, "cost_usd")),
                    js_str(value, "costCurrency"),
                    js_f64(value, "sellINR").or(js_f64(value, "sell_inr")),
                    js_f64(value, "baseFX").or(js_f64(value, "base_fx")),
                    js_bool_int(value, "isReseller"),
                    js_str(value, "resellerName"),
                    js_str(value, "commercialModel"),
                    js_str(value, "sourcedViaResellerKey"),
                    js_str(value, "renewalStatus"),
                    raw,
                    now_iso(),
                ],
            )?;
            count += 1;
        }
        tx.commit()?;
        Ok(count)
    }) {
        Ok(n) => SaveResult::ok(n),
        Err(e) => SaveResult::err(e),
    }
}

// ----------------------------------------------------------------------------
// Prospects — array from ms_prospects_v1
// ----------------------------------------------------------------------------
#[tauri::command]
fn storage_save_prospects(state: tauri::State<DbState>, payload: String) -> SaveResult {
    let parsed: serde_json::Value = match serde_json::from_str(&payload) {
        Ok(v) => v,
        Err(e) => return SaveResult::err(format!("Invalid JSON: {e}")),
    };
    let arr = match parsed.as_array() {
        Some(a) => a,
        None => return SaveResult::err("Expected array".into()),
    };

    match with_writer(&state, |con| {
        let tx = con.transaction()?;
        tx.execute("DELETE FROM prospects", [])?;
        let mut count = 0usize;
        for value in arr {
            let raw = value.to_string();
            let id = match js_str(value, "id") {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => continue, // skip rows without id (shouldn't happen)
            };
            tx.execute(
                "INSERT INTO prospects (
                    id, company, opp_name, stage, priority, source, owner,
                    acv, licenses, term_months, currency, commercial_model,
                    sourced_via_reseller_key, close_date, actual_close_date, start_date,
                    archived, raw_data, created_at, updated_at, modified_at
                ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21)",
                params![
                    id,
                    js_str(value, "company").unwrap_or(""),
                    js_str(value, "oppName"),
                    js_str(value, "stage"),
                    js_str(value, "priority"),
                    js_str(value, "source"),
                    js_str(value, "owner"),
                    js_f64(value, "acv"),
                    js_i64(value, "licenses"),
                    js_i64(value, "termMonths"),
                    js_str(value, "currency"),
                    js_str(value, "commercialModel"),
                    js_str(value, "sourcedViaResellerKey"),
                    js_str(value, "closeDate"),
                    js_str(value, "actualCloseDate"),
                    js_str(value, "startDate"),
                    js_bool_int(value, "archived"),
                    raw,
                    js_str(value, "createdAt"),
                    js_str(value, "updatedAt"),
                    now_iso(),
                ],
            )?;
            count += 1;
        }
        tx.commit()?;
        Ok(count)
    }) {
        Ok(n) => SaveResult::ok(n),
        Err(e) => SaveResult::err(e),
    }
}

// ----------------------------------------------------------------------------
// Proposals — array from ms_proposals_v1 (currently empty source)
// ----------------------------------------------------------------------------
#[tauri::command]
fn storage_save_proposals(state: tauri::State<DbState>, payload: String) -> SaveResult {
    let parsed: serde_json::Value = match serde_json::from_str(&payload) {
        Ok(v) => v,
        Err(e) => return SaveResult::err(format!("Invalid JSON: {e}")),
    };
    let arr = match parsed.as_array() {
        Some(a) => a,
        None => return SaveResult::err("Expected array".into()),
    };

    match with_writer(&state, |con| {
        let tx = con.transaction()?;
        tx.execute("DELETE FROM proposals", [])?;
        let mut count = 0usize;
        for value in arr {
            let raw = value.to_string();
            let id = match js_str(value, "id") {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => continue,
            };
            tx.execute(
                "INSERT INTO proposals (
                    id, proposal_number, client_name, proposal_date, valid_until,
                    status, grand_total, commercial_model, sourced_via_reseller_key,
                    raw_data, modified_at
                ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                params![
                    id,
                    js_str(value, "proposalNumber"),
                    js_str(value, "clientName"),
                    js_str(value, "proposalDate"),
                    js_str(value, "validUntil"),
                    js_str(value, "status"),
                    js_f64(value, "grandTotal"),
                    js_str(value, "commercialModel"),
                    js_str(value, "sourcedViaResellerKey"),
                    raw,
                    now_iso(),
                ],
            )?;
            count += 1;
        }
        tx.commit()?;
        Ok(count)
    }) {
        Ok(n) => SaveResult::ok(n),
        Err(e) => SaveResult::err(e),
    }
}

// ----------------------------------------------------------------------------
// Purchase Orders — array from ms_purchase_orders_v1 (currently empty source)
// ----------------------------------------------------------------------------
#[tauri::command]
fn storage_save_purchase_orders(state: tauri::State<DbState>, payload: String) -> SaveResult {
    let parsed: serde_json::Value = match serde_json::from_str(&payload) {
        Ok(v) => v,
        Err(e) => return SaveResult::err(format!("Invalid JSON: {e}")),
    };
    let arr = match parsed.as_array() {
        Some(a) => a,
        None => return SaveResult::err("Expected array".into()),
    };

    match with_writer(&state, |con| {
        let tx = con.transaction()?;
        tx.execute("DELETE FROM purchase_orders", [])?;
        let mut count = 0usize;
        for value in arr {
            let raw = value.to_string();
            let id = match js_str(value, "id") {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => continue,
            };
            tx.execute(
                "INSERT INTO purchase_orders (
                    id, po_number, vendor_name, po_date, status, grand_total,
                    raw_data, modified_at
                ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    id,
                    js_str(value, "poNumber"),
                    js_str(value, "vendorName"),
                    js_str(value, "poDate"),
                    js_str(value, "status"),
                    js_f64(value, "grandTotal"),
                    raw,
                    now_iso(),
                ],
            )?;
            count += 1;
        }
        tx.commit()?;
        Ok(count)
    }) {
        Ok(n) => SaveResult::ok(n),
        Err(e) => SaveResult::err(e),
    }
}

// ----------------------------------------------------------------------------
// Invoices — array from ms_invoices_v1
// ----------------------------------------------------------------------------
#[tauri::command]
fn storage_save_invoices(state: tauri::State<DbState>, payload: String) -> SaveResult {
    let parsed: serde_json::Value = match serde_json::from_str(&payload) {
        Ok(v) => v,
        Err(e) => return SaveResult::err(format!("Invalid JSON: {e}")),
    };
    let arr = match parsed.as_array() {
        Some(a) => a,
        None => return SaveResult::err("Expected array".into()),
    };

    match with_writer(&state, |con| {
        let tx = con.transaction()?;
        tx.execute("DELETE FROM invoices", [])?;
        let mut count = 0usize;
        for value in arr {
            let raw = value.to_string();
            let id = match js_str(value, "id") {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => continue,
            };
            // linkedContractIdx may be number or string in the wild; coerce to text.
            let linked_idx = match value.get("linkedContractIdx") {
                Some(serde_json::Value::Number(n)) => Some(n.to_string()),
                Some(serde_json::Value::String(s)) => Some(s.clone()),
                _ => None,
            };
            let linked_cycle = match value.get("linkedCycleYearIdx") {
                Some(serde_json::Value::Number(n)) => Some(n.to_string()),
                Some(serde_json::Value::String(s)) => Some(s.clone()),
                _ => None,
            };
            tx.execute(
                "INSERT INTO invoices (
                    id, invoice_number, invoice_date, due_date, client_name, status,
                    gst_mode, place_of_supply_code, grand_total, gross_total,
                    discount_total, gst_total, cgst, sgst, igst,
                    amount_paid, amount_outstanding, paid_at, cancelled_at,
                    linked_contract_idx, linked_cycle_year_idx, linked_proposal_id,
                    commercial_model, sourced_via_reseller_key, raw_data,
                    created_at, updated_at, issued_at, modified_at
                ) VALUES (
                    ?1,?2,?3,?4,?5,?6, ?7,?8,?9,?10, ?11,?12,?13,?14,?15,
                    ?16,?17,?18,?19, ?20,?21,?22, ?23,?24,?25, ?26,?27,?28,?29
                )",
                params![
                    id,
                    js_str(value, "invoiceNumber"),
                    js_str(value, "invoiceDate"),
                    js_str(value, "dueDate"),
                    js_str(value, "clientName"),
                    js_str(value, "status"),
                    js_str(value, "gstMode"),
                    js_str(value, "placeOfSupplyCode"),
                    js_f64(value, "grandTotal"),
                    js_f64(value, "grossTotal"),
                    js_f64(value, "discountTotal"),
                    js_f64(value, "gstTotal"),
                    js_f64(value, "cgst"),
                    js_f64(value, "sgst"),
                    js_f64(value, "igst"),
                    js_f64(value, "amountPaid").unwrap_or(0.0),
                    js_f64(value, "amountOutstanding"),
                    js_str(value, "paidAt"),
                    js_str(value, "cancelledAt"),
                    linked_idx,
                    linked_cycle,
                    js_str(value, "linkedProposalId"),
                    js_str(value, "commercialModel"),
                    js_str(value, "sourcedViaResellerKey"),
                    raw,
                    js_str(value, "createdAt"),
                    js_str(value, "updatedAt"),
                    js_str(value, "issuedAt"),
                    now_iso(),
                ],
            )?;
            count += 1;
        }
        tx.commit()?;
        Ok(count)
    }) {
        Ok(n) => SaveResult::ok(n),
        Err(e) => SaveResult::err(e),
    }
}

// ----------------------------------------------------------------------------
// Commissions — { accruals: [...], payouts: [...] } from ms_commissions_v1
// ----------------------------------------------------------------------------
#[tauri::command]
fn storage_save_commissions(state: tauri::State<DbState>, payload: String) -> SaveResult {
    let parsed: serde_json::Value = match serde_json::from_str(&payload) {
        Ok(v) => v,
        Err(e) => return SaveResult::err(format!("Invalid JSON: {e}")),
    };
    let obj = match parsed.as_object() {
        Some(o) => o,
        None => return SaveResult::err("Expected object with accruals + payouts".into()),
    };
    let empty: Vec<serde_json::Value> = vec![];
    let accruals = obj.get("accruals").and_then(|v| v.as_array()).unwrap_or(&empty);
    let payouts = obj.get("payouts").and_then(|v| v.as_array()).unwrap_or(&empty);

    match with_writer(&state, |con| {
        let tx = con.transaction()?;
        tx.execute("DELETE FROM commission_accruals", [])?;
        tx.execute("DELETE FROM commission_payouts", [])?;
        let mut count = 0usize;
        for value in accruals {
            let raw = value.to_string();
            let id = match js_str(value, "id") {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => continue,
            };
            tx.execute(
                "INSERT INTO commission_accruals (
                    id, reseller_key, invoice_id, invoice_number, client_name, commercial_model,
                    commission_base_inr, commission_pct, commission_amount_inr,
                    accrual_date, invoice_paid_date, source_contract_idx, backfilled,
                    raw_data, created_at, modified_at
                ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
                params![
                    id,
                    js_str(value, "resellerKey"),
                    js_str(value, "invoiceId"),
                    js_str(value, "invoiceNumber"),
                    js_str(value, "clientName"),
                    js_str(value, "commercialModel"),
                    js_f64(value, "commissionBaseINR"),
                    js_f64(value, "commissionPct"),
                    js_f64(value, "commissionAmountINR"),
                    js_str(value, "accrualDate"),
                    js_str(value, "invoicePaidDate"),
                    js_i64(value, "sourceContractIdx"),
                    js_bool_int(value, "backfilled"),
                    raw,
                    js_str(value, "createdAt"),
                    now_iso(),
                ],
            )?;
            count += 1;
        }
        for value in payouts {
            let raw = value.to_string();
            let id = match js_str(value, "id") {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => continue,
            };
            // accrualIds is an array; serialize to JSON string for the column.
            let accrual_ids = value.get("accrualIds")
                .map(|v| v.to_string())
                .unwrap_or_else(|| "[]".to_string());
            tx.execute(
                "INSERT INTO commission_payouts (
                    id, reseller_key, payout_date, amount_inr, accrual_ids,
                    raw_data, created_at, modified_at
                ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    id,
                    js_str(value, "resellerKey"),
                    js_str(value, "payoutDate"),
                    js_f64(value, "amountINR"),
                    accrual_ids,
                    raw,
                    js_str(value, "createdAt"),
                    now_iso(),
                ],
            )?;
            count += 1;
        }
        tx.commit()?;
        Ok(count)
    }) {
        Ok(n) => SaveResult::ok(n),
        Err(e) => SaveResult::err(e),
    }
}

// ----------------------------------------------------------------------------
// Company Profile — single dict from ms_company_profile_v1
// ----------------------------------------------------------------------------
#[tauri::command]
fn storage_save_company_profile(state: tauri::State<DbState>, payload: String) -> SaveResult {
    let parsed: serde_json::Value = match serde_json::from_str(&payload) {
        Ok(v) => v,
        Err(e) => return SaveResult::err(format!("Invalid JSON: {e}")),
    };
    if !parsed.is_object() {
        return SaveResult::err("Expected object".into());
    }
    let value = &parsed;
    let raw = value.to_string();

    match with_writer(&state, |con| {
        // Delete + insert (CHECK constraint enforces id = 1).
        con.execute("DELETE FROM company_profile", [])?;
        con.execute(
            "INSERT INTO company_profile (
                id, legal_name, trading_name, gstin, state_code, pan, cin,
                logo_data_url, letterhead_data_url, signature_image_data_url,
                letterhead_use_as_background,
                invoice_prefix, proposal_prefix, po_prefix,
                invoice_next_number, proposal_next_number, po_next_number,
                numbering_mode,
                bank_name, bank_branch, bank_account_holder, bank_account_number, bank_ifsc,
                signatory_name, signatory_designation,
                raw_data, modified_at
            ) VALUES (
                1, ?1,?2,?3,?4,?5,?6, ?7,?8,?9, ?10,
                ?11,?12,?13, ?14,?15,?16, ?17,
                ?18,?19,?20,?21,?22, ?23,?24, ?25, ?26
            )",
            params![
                js_str(value, "legalName"),
                js_str(value, "tradingName"),
                js_str(value, "gstin"),
                js_str(value, "stateCode"),
                js_str(value, "pan"),
                js_str(value, "cin"),
                js_str(value, "logoDataUrl"),
                js_str(value, "letterheadDataUrl"),
                js_str(value, "signatureImageDataUrl"),
                js_bool_int(value, "letterheadUseAsBackground"),
                js_str(value, "invoicePrefix"),
                js_str(value, "proposalPrefix"),
                js_str(value, "poPrefix"),
                js_i64(value, "invoiceNextNumber"),
                js_i64(value, "proposalNextNumber"),
                js_i64(value, "poNextNumber"),
                js_str(value, "numberingMode"),
                js_str(value, "bankName"),
                js_str(value, "bankBranch"),
                js_str(value, "bankAccountHolder"),
                js_str(value, "bankAccountNumber"),
                js_str(value, "bankIfsc"),
                js_str(value, "signatoryName"),
                js_str(value, "signatoryDesignation"),
                raw,
                now_iso(),
            ],
        )?;
        Ok(1usize)
    }) {
        Ok(n) => SaveResult::ok(n),
        Err(e) => SaveResult::err(e),
    }
}

// ----------------------------------------------------------------------------
// Attachments — array from ms_attachments_v1
// (Currently empty source, but write path exists for future entries.)
// ----------------------------------------------------------------------------
#[tauri::command]
fn storage_save_attachments(state: tauri::State<DbState>, payload: String) -> SaveResult {
    let parsed: serde_json::Value = match serde_json::from_str(&payload) {
        Ok(v) => v,
        Err(e) => return SaveResult::err(format!("Invalid JSON: {e}")),
    };
    let arr = match parsed.as_array() {
        Some(a) => a,
        None => return SaveResult::err("Expected array".into()),
    };

    match with_writer(&state, |con| {
        let tx = con.transaction()?;
        tx.execute("DELETE FROM attachments", [])?;
        let mut count = 0usize;
        for value in arr {
            let raw = value.to_string();
            let id = match js_str(value, "id") {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => continue,
            };
            tx.execute(
                "INSERT INTO attachments (
                    id, related_entity_type, related_entity_id, filename, mime_type,
                    size_bytes, file_path, fallback_url, uploaded_at, raw_data, modified_at
                ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                params![
                    id,
                    js_str(value, "relatedEntityType"),
                    js_str(value, "relatedEntityId"),
                    js_str(value, "filename"),
                    js_str(value, "mimeType"),
                    js_i64(value, "sizeBytes"),
                    js_str(value, "filePath"),
                    js_str(value, "fallbackUrl"),
                    js_str(value, "uploadedAt"),
                    raw,
                    now_iso(),
                ],
            )?;
            count += 1;
        }
        tx.commit()?;
        Ok(count)
    }) {
        Ok(n) => SaveResult::ok(n),
        Err(e) => SaveResult::err(e),
    }
}

// ----------------------------------------------------------------------------
// App Settings — single key/value, used for theme, view prefs, etc.
// Maps the localStorage key (ms_theme, ms_fy_basis_v1, etc.) to the
// app_settings.key column.
// ----------------------------------------------------------------------------
#[tauri::command]
fn storage_save_setting(state: tauri::State<DbState>, ls_key: String, value: String) -> SaveResult {
    let setting_key = match ls_key.as_str() {
        "ms_theme"                 => "theme",
        "ms_fy_basis_v1"           => "fy_basis",
        "ms_invoices_view_v1"      => "invoices_view",
        "ms_pipeline_view_v1"      => "pipeline_view",
        "ms_proposals_view_v1"     => "proposals_view",
        "ms_schema_meta_v1"        => "schema_meta",
        "ms_migration_4A_state_v1" => "migration_4A_state",
        _ => return SaveResult::err(format!("Unknown setting key: {ls_key}")),
    };

    match with_writer(&state, |con| {
        con.execute(
            "INSERT INTO app_settings (key, value, modified_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, modified_at = excluded.modified_at",
            params![setting_key, value, now_iso()],
        )?;
        Ok(1usize)
    }) {
        Ok(n) => SaveResult::ok(n),
        Err(e) => SaveResult::err(e),
    }
}

// ============================================================================
// First-launch wizard support — given a path the user picked, copy the file
// into App Support and reset our connection so the next ensure_open uses it.
// ============================================================================

#[derive(Serialize)]
struct InstallDbResult {
    ok: bool,
    error: Option<String>,
    db_path: Option<String>,
}

#[tauri::command]
fn install_db_from_path(state: tauri::State<DbState>, source_path: String) -> InstallDbResult {
    let target = match db_path() {
        Some(p) => p,
        None => return InstallDbResult {
            ok: false,
            error: Some("Could not resolve App Support directory".into()),
            db_path: None,
        },
    };
    ensure_parent_exists(&target);

    // Quick sanity check: confirm the file is actually a SQLite database with
    // our schema (look for the clients table). This catches the "user picked
    // the wrong file" case before we overwrite anything.
    {
        let probe = match Connection::open_with_flags(&source_path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
            Ok(c) => c,
            Err(e) => return InstallDbResult {
                ok: false,
                error: Some(format!("Selected file isn't a readable SQLite database: {e}")),
                db_path: Some(target.to_string_lossy().to_string()),
            },
        };
        let tables_ok: Result<i64, _> = probe.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN
             ('clients','contracts','invoices','company_profile','app_settings')",
            [],
            |r| r.get(0),
        );
        match tables_ok {
            Ok(n) if n >= 5 => { /* good */ }
            _ => return InstallDbResult {
                ok: false,
                error: Some("Selected file is a SQLite database but doesn't look like a Masterstone DB (missing core tables)".into()),
                db_path: Some(target.to_string_lossy().to_string()),
            },
        }
    }

    if let Err(e) = std::fs::copy(&source_path, &target) {
        return InstallDbResult {
            ok: false,
            error: Some(format!("Could not copy file into App Support: {e}")),
            db_path: Some(target.to_string_lossy().to_string()),
        };
    }

    // Force the next ensure_open to pick up the new file.
    state.reset();

    InstallDbResult {
        ok: true,
        error: None,
        db_path: Some(target.to_string_lossy().to_string()),
    }
}

// ============================================================================
// Entry point
// ============================================================================

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(DbState::new())
        .invoke_handler(tauri::generate_handler![
            storage_load_all,
            storage_save_clients,
            storage_save_resellers,
            storage_save_oems,
            storage_save_contracts,
            storage_save_prospects,
            storage_save_proposals,
            storage_save_purchase_orders,
            storage_save_invoices,
            storage_save_commissions,
            storage_save_company_profile,
            storage_save_attachments,
            storage_save_setting,
            install_db_from_path,
        ])
        .setup(|_app| Ok(()))
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
