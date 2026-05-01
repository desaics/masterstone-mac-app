// Masterstone CRM — Tauri runtime entry.
//
// Session 5 scope:
//   1. Replace broken `<a target=_blank>` behavior with the opener plugin.
//      JS intercepts OneDrive link clicks and routes them through
//      `open_external_url` (Rust → opener::open_url → default browser).
//   2. iPhone read-only HTML snapshot generator. Writes to
//      ~/OneDrive/Masterstone/Masterstone_Snapshot_YYYY-MM-DD.html on quit
//      (only if data changed since last snapshot). Keeps last 7, auto-purges.
//   3. Reveal-in-Finder helper for the OneDrive folder.

use rusqlite::{params, Connection, OpenFlags};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use tauri_plugin_opener::OpenerExt;

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

/// Path to the OneDrive snapshot folder. Returns None if no usable OneDrive
/// folder can be found on the user's Mac.
///
/// Bug fix #49 (Session 5 hotfix v1) — modern macOS OneDrive client (especially
/// multi-account setups) installs to ~/Library/CloudStorage/OneDrive-* rather
/// than the legacy ~/OneDrive symlink. The original implementation only looked
/// for ~/OneDrive, which caused snapshot generation to fail silently on
/// machines with three OneDrive accounts.
///
/// User has three OneDrive accounts: ACAPartnersLLP, Masterstone, Personal.
/// Snapshots target the **Masterstone** business account so the data lives
/// alongside other Masterstone artefacts. The "Masterstone" subfolder name is
/// kept under the OneDrive-Masterstone root just for tidiness (snapshots in
/// their own subfolder rather than mixed with other root-level files).
///
/// Resolution priority:
///   1. ~/Library/CloudStorage/OneDrive-Masterstone/Masterstone/  ← target
///   2. ~/OneDrive/Masterstone/   (legacy symlink fallback)
///   3. None — surfaced to the user as "OneDrive folder not found"
fn snapshot_dir() -> Option<PathBuf> {
    let home = dirs::home_dir()?;

    // 1. Modern multi-account macOS layout — preferred.
    let cloud_target = home
        .join("Library")
        .join("CloudStorage")
        .join("OneDrive-Masterstone");
    if cloud_target.is_dir() {
        return Some(cloud_target.join("Masterstone"));
    }

    // 2. Legacy symlink fallback (single-account installs, older macOS).
    let legacy = home.join("OneDrive");
    if legacy.is_dir() {
        return Some(legacy.join("Masterstone"));
    }

    None
}

// ============================================================================
// State
// ============================================================================

struct DbState {
    conn: Mutex<Option<Connection>>,
}

impl DbState {
    fn new() -> Self {
        Self { conn: Mutex::new(None) }
    }

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
        let _: String = con.query_row("PRAGMA journal_mode = WAL;", [], |r| r.get(0))
            .map_err(|e| format!("Could not enable WAL: {e}"))?;
        con.execute("PRAGMA foreign_keys = ON;", [])
            .map_err(|e| format!("Could not enable foreign keys: {e}"))?;
        *guard = Some(con);
        Ok(())
    }

    fn reset(&self) {
        if let Ok(mut guard) = self.conn.lock() {
            *guard = None;
        }
    }
}

// ============================================================================
// Read path — unchanged from Session 4
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
    let con = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;

    let mut data: BTreeMap<String, String> = BTreeMap::new();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();

    let contracts = collect_array(&con, "SELECT raw_data FROM contracts ORDER BY legacy_idx ASC")?;
    counts.insert("contracts".into(), contracts.len());
    data.insert("ms_pro_v210".into(), serde_json::Value::Array(contracts).to_string());

    let clients_dict = collect_dict_by_column(&con, "SELECT company_name, raw_data FROM clients")?;
    counts.insert("clients".into(), clients_dict.len());
    data.insert("ms_client_master_v1".into(), serde_json::Value::Object(clients_dict).to_string());

    let resellers_dict = collect_dict_by_column(&con, "SELECT company_name, raw_data FROM resellers")?;
    counts.insert("resellers".into(), resellers_dict.len());
    data.insert("ms_reseller_master_v1".into(), serde_json::Value::Object(resellers_dict).to_string());

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

    let invoices = collect_array(&con, "SELECT raw_data FROM invoices ORDER BY created_at, id")?;
    counts.insert("invoices".into(), invoices.len());
    data.insert("ms_invoices_v1".into(), serde_json::Value::Array(invoices).to_string());

    let prospects = collect_array(&con, "SELECT raw_data FROM prospects ORDER BY created_at, id")?;
    counts.insert("prospects".into(), prospects.len());
    data.insert("ms_prospects_v1".into(), serde_json::Value::Array(prospects).to_string());

    let proposals = collect_array(&con, "SELECT raw_data FROM proposals ORDER BY id")?;
    counts.insert("proposals".into(), proposals.len());
    data.insert("ms_proposals_v1".into(), serde_json::Value::Array(proposals).to_string());

    let pos = collect_array(&con, "SELECT raw_data FROM purchase_orders ORDER BY id")?;
    counts.insert("purchase_orders".into(), pos.len());
    data.insert("ms_purchase_orders_v1".into(), serde_json::Value::Array(pos).to_string());

    let accruals = collect_array(&con, "SELECT raw_data FROM commission_accruals ORDER BY accrual_date, id")?;
    let payouts = collect_array(&con, "SELECT raw_data FROM commission_payouts ORDER BY payout_date, id")?;
    counts.insert("commission_accruals".into(), accruals.len());
    counts.insert("commission_payouts".into(), payouts.len());
    let mut commissions_obj = serde_json::Map::new();
    commissions_obj.insert("accruals".into(), serde_json::Value::Array(accruals));
    commissions_obj.insert("payouts".into(), serde_json::Value::Array(payouts));
    data.insert("ms_commissions_v1".into(), serde_json::Value::Object(commissions_obj).to_string());

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

    let attachments = collect_array(&con, "SELECT raw_data FROM attachments ORDER BY uploaded_at, id")?;
    counts.insert("attachments".into(), attachments.len());
    data.insert("ms_attachments_v1".into(), serde_json::Value::Array(attachments).to_string());

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
// Write path — unchanged from Session 4 (helpers and 12 commands inlined)
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

fn js_str<'a>(v: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str())
}
fn js_f64(v: &serde_json::Value, key: &str) -> Option<f64> {
    let val = v.get(key)?;
    if let Some(n) = val.as_f64() { return Some(n); }
    if let Some(s) = val.as_str() { return s.parse::<f64>().ok(); }
    None
}
fn js_i64(v: &serde_json::Value, key: &str) -> Option<i64> {
    let val = v.get(key)?;
    if let Some(n) = val.as_i64() { return Some(n); }
    if let Some(s) = val.as_str() { return s.parse::<i64>().ok(); }
    None
}
fn js_bool_int(v: &serde_json::Value, key: &str) -> i64 {
    match v.get(key) {
        Some(serde_json::Value::Bool(b)) => if *b { 1 } else { 0 },
        Some(serde_json::Value::Number(n)) => if n.as_f64().unwrap_or(0.0) != 0.0 { 1 } else { 0 },
        _ => 0,
    }
}

fn with_writer<F, R>(state: &DbState, f: F) -> Result<R, String>
where F: FnOnce(&mut Connection) -> rusqlite::Result<R>,
{
    state.ensure_open()?;
    let mut guard = state.conn.lock().map_err(|e| format!("Mutex poisoned: {e}"))?;
    let con = guard.as_mut().ok_or("DB connection unavailable")?;
    f(con).map_err(|e| format!("{e}"))
}

#[tauri::command]
fn storage_save_clients(state: tauri::State<DbState>, payload: String) -> SaveResult {
    let parsed: serde_json::Value = match serde_json::from_str(&payload) {
        Ok(v) => v, Err(e) => return SaveResult::err(format!("Invalid JSON: {e}")),
    };
    let obj = match parsed.as_object() {
        Some(o) => o, None => return SaveResult::err("Expected object".into()),
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
                    js_str(value, "shortName"), js_str(value, "industry"),
                    js_str(value, "tier"), js_str(value, "status"),
                    js_str(value, "gstin"), js_str(value, "pan"),
                    js_str(value, "stateCode"), js_bool_int(value, "isReseller"),
                    js_str(value, "resellerName"), js_str(value, "vendorCode"),
                    raw, js_str(value, "createdAt"), js_str(value, "updatedAt"),
                    now_iso(),
                ],
            )?;
            count += 1;
        }
        tx.commit()?;
        Ok(count)
    }) { Ok(n) => SaveResult::ok(n), Err(e) => SaveResult::err(e) }
}

#[tauri::command]
fn storage_save_resellers(state: tauri::State<DbState>, payload: String) -> SaveResult {
    let parsed: serde_json::Value = match serde_json::from_str(&payload) {
        Ok(v) => v, Err(e) => return SaveResult::err(format!("Invalid JSON: {e}")),
    };
    let obj = match parsed.as_object() {
        Some(o) => o, None => return SaveResult::err("Expected object".into()),
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
                    name, js_str(value, "shortName"), js_str(value, "status"),
                    js_str(value, "gstin"), js_str(value, "pan"),
                    js_str(value, "stateCode"), js_bool_int(value, "needsCompletion"),
                    raw, js_str(value, "createdAt"), js_str(value, "updatedAt"), now_iso(),
                ],
            )?;
            count += 1;
        }
        tx.commit()?;
        Ok(count)
    }) { Ok(n) => SaveResult::ok(n), Err(e) => SaveResult::err(e) }
}

#[tauri::command]
fn storage_save_oems(state: tauri::State<DbState>, payload: String) -> SaveResult {
    let parsed: serde_json::Value = match serde_json::from_str(&payload) {
        Ok(v) => v, Err(e) => return SaveResult::err(format!("Invalid JSON: {e}")),
    };
    let obj = match parsed.as_object() {
        Some(o) => o, None => return SaveResult::err("Expected object".into()),
    };
    match with_writer(&state, |con| {
        let tx = con.transaction()?;
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
    }) { Ok(n) => SaveResult::ok(n), Err(e) => SaveResult::err(e) }
}

#[tauri::command]
fn storage_save_contracts(state: tauri::State<DbState>, payload: String) -> SaveResult {
    let parsed: serde_json::Value = match serde_json::from_str(&payload) {
        Ok(v) => v, Err(e) => return SaveResult::err(format!("Invalid JSON: {e}")),
    };
    let arr = match parsed.as_array() {
        Some(a) => a, None => return SaveResult::err("Expected array of contracts".into()),
    };
    match with_writer(&state, |con| {
        let tx = con.transaction()?;
        tx.execute("DELETE FROM contracts", [])?;
        let mut count = 0usize;
        for (idx, value) in arr.iter().enumerate() {
            let raw = value.to_string();
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
                    id, idx as i64,
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
                    raw, now_iso(),
                ],
            )?;
            count += 1;
        }
        tx.commit()?;
        Ok(count)
    }) { Ok(n) => SaveResult::ok(n), Err(e) => SaveResult::err(e) }
}

#[tauri::command]
fn storage_save_prospects(state: tauri::State<DbState>, payload: String) -> SaveResult {
    let parsed: serde_json::Value = match serde_json::from_str(&payload) {
        Ok(v) => v, Err(e) => return SaveResult::err(format!("Invalid JSON: {e}")),
    };
    let arr = match parsed.as_array() {
        Some(a) => a, None => return SaveResult::err("Expected array".into()),
    };
    match with_writer(&state, |con| {
        let tx = con.transaction()?;
        tx.execute("DELETE FROM prospects", [])?;
        let mut count = 0usize;
        for value in arr {
            let raw = value.to_string();
            let id = match js_str(value, "id") {
                Some(s) if !s.is_empty() => s.to_string(), _ => continue,
            };
            tx.execute(
                "INSERT INTO prospects (
                    id, company, opp_name, stage, priority, source, owner,
                    acv, licenses, term_months, currency, commercial_model,
                    sourced_via_reseller_key, close_date, actual_close_date, start_date,
                    archived, raw_data, created_at, updated_at, modified_at
                ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21)",
                params![
                    id, js_str(value, "company").unwrap_or(""),
                    js_str(value, "oppName"), js_str(value, "stage"),
                    js_str(value, "priority"), js_str(value, "source"),
                    js_str(value, "owner"), js_f64(value, "acv"),
                    js_i64(value, "licenses"), js_i64(value, "termMonths"),
                    js_str(value, "currency"), js_str(value, "commercialModel"),
                    js_str(value, "sourcedViaResellerKey"),
                    js_str(value, "closeDate"), js_str(value, "actualCloseDate"),
                    js_str(value, "startDate"), js_bool_int(value, "archived"),
                    raw, js_str(value, "createdAt"), js_str(value, "updatedAt"), now_iso(),
                ],
            )?;
            count += 1;
        }
        tx.commit()?;
        Ok(count)
    }) { Ok(n) => SaveResult::ok(n), Err(e) => SaveResult::err(e) }
}

#[tauri::command]
fn storage_save_proposals(state: tauri::State<DbState>, payload: String) -> SaveResult {
    let parsed: serde_json::Value = match serde_json::from_str(&payload) {
        Ok(v) => v, Err(e) => return SaveResult::err(format!("Invalid JSON: {e}")),
    };
    let arr = match parsed.as_array() {
        Some(a) => a, None => return SaveResult::err("Expected array".into()),
    };
    match with_writer(&state, |con| {
        let tx = con.transaction()?;
        tx.execute("DELETE FROM proposals", [])?;
        let mut count = 0usize;
        for value in arr {
            let raw = value.to_string();
            let id = match js_str(value, "id") {
                Some(s) if !s.is_empty() => s.to_string(), _ => continue,
            };
            tx.execute(
                "INSERT INTO proposals (
                    id, proposal_number, client_name, proposal_date, valid_until,
                    status, grand_total, commercial_model, sourced_via_reseller_key,
                    raw_data, modified_at
                ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                params![
                    id, js_str(value, "proposalNumber"), js_str(value, "clientName"),
                    js_str(value, "proposalDate"), js_str(value, "validUntil"),
                    js_str(value, "status"), js_f64(value, "grandTotal"),
                    js_str(value, "commercialModel"), js_str(value, "sourcedViaResellerKey"),
                    raw, now_iso(),
                ],
            )?;
            count += 1;
        }
        tx.commit()?;
        Ok(count)
    }) { Ok(n) => SaveResult::ok(n), Err(e) => SaveResult::err(e) }
}

#[tauri::command]
fn storage_save_purchase_orders(state: tauri::State<DbState>, payload: String) -> SaveResult {
    let parsed: serde_json::Value = match serde_json::from_str(&payload) {
        Ok(v) => v, Err(e) => return SaveResult::err(format!("Invalid JSON: {e}")),
    };
    let arr = match parsed.as_array() {
        Some(a) => a, None => return SaveResult::err("Expected array".into()),
    };
    match with_writer(&state, |con| {
        let tx = con.transaction()?;
        tx.execute("DELETE FROM purchase_orders", [])?;
        let mut count = 0usize;
        for value in arr {
            let raw = value.to_string();
            let id = match js_str(value, "id") {
                Some(s) if !s.is_empty() => s.to_string(), _ => continue,
            };
            tx.execute(
                "INSERT INTO purchase_orders (
                    id, po_number, vendor_name, po_date, status, grand_total,
                    raw_data, modified_at
                ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    id, js_str(value, "poNumber"), js_str(value, "vendorName"),
                    js_str(value, "poDate"), js_str(value, "status"),
                    js_f64(value, "grandTotal"), raw, now_iso(),
                ],
            )?;
            count += 1;
        }
        tx.commit()?;
        Ok(count)
    }) { Ok(n) => SaveResult::ok(n), Err(e) => SaveResult::err(e) }
}

#[tauri::command]
fn storage_save_invoices(state: tauri::State<DbState>, payload: String) -> SaveResult {
    let parsed: serde_json::Value = match serde_json::from_str(&payload) {
        Ok(v) => v, Err(e) => return SaveResult::err(format!("Invalid JSON: {e}")),
    };
    let arr = match parsed.as_array() {
        Some(a) => a, None => return SaveResult::err("Expected array".into()),
    };
    match with_writer(&state, |con| {
        let tx = con.transaction()?;
        tx.execute("DELETE FROM invoices", [])?;
        let mut count = 0usize;
        for value in arr {
            let raw = value.to_string();
            let id = match js_str(value, "id") {
                Some(s) if !s.is_empty() => s.to_string(), _ => continue,
            };
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
                    id, js_str(value, "invoiceNumber"),
                    js_str(value, "invoiceDate"), js_str(value, "dueDate"),
                    js_str(value, "clientName"), js_str(value, "status"),
                    js_str(value, "gstMode"), js_str(value, "placeOfSupplyCode"),
                    js_f64(value, "grandTotal"), js_f64(value, "grossTotal"),
                    js_f64(value, "discountTotal"), js_f64(value, "gstTotal"),
                    js_f64(value, "cgst"), js_f64(value, "sgst"), js_f64(value, "igst"),
                    js_f64(value, "amountPaid").unwrap_or(0.0),
                    js_f64(value, "amountOutstanding"),
                    js_str(value, "paidAt"), js_str(value, "cancelledAt"),
                    linked_idx, linked_cycle, js_str(value, "linkedProposalId"),
                    js_str(value, "commercialModel"), js_str(value, "sourcedViaResellerKey"),
                    raw, js_str(value, "createdAt"),
                    js_str(value, "updatedAt"), js_str(value, "issuedAt"), now_iso(),
                ],
            )?;
            count += 1;
        }
        tx.commit()?;
        Ok(count)
    }) { Ok(n) => SaveResult::ok(n), Err(e) => SaveResult::err(e) }
}

#[tauri::command]
fn storage_save_commissions(state: tauri::State<DbState>, payload: String) -> SaveResult {
    let parsed: serde_json::Value = match serde_json::from_str(&payload) {
        Ok(v) => v, Err(e) => return SaveResult::err(format!("Invalid JSON: {e}")),
    };
    let obj = match parsed.as_object() {
        Some(o) => o, None => return SaveResult::err("Expected object".into()),
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
                Some(s) if !s.is_empty() => s.to_string(), _ => continue,
            };
            tx.execute(
                "INSERT INTO commission_accruals (
                    id, reseller_key, invoice_id, invoice_number, client_name, commercial_model,
                    commission_base_inr, commission_pct, commission_amount_inr,
                    accrual_date, invoice_paid_date, source_contract_idx, backfilled,
                    raw_data, created_at, modified_at
                ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
                params![
                    id, js_str(value, "resellerKey"), js_str(value, "invoiceId"),
                    js_str(value, "invoiceNumber"), js_str(value, "clientName"),
                    js_str(value, "commercialModel"), js_f64(value, "commissionBaseINR"),
                    js_f64(value, "commissionPct"), js_f64(value, "commissionAmountINR"),
                    js_str(value, "accrualDate"), js_str(value, "invoicePaidDate"),
                    js_i64(value, "sourceContractIdx"), js_bool_int(value, "backfilled"),
                    raw, js_str(value, "createdAt"), now_iso(),
                ],
            )?;
            count += 1;
        }
        for value in payouts {
            let raw = value.to_string();
            let id = match js_str(value, "id") {
                Some(s) if !s.is_empty() => s.to_string(), _ => continue,
            };
            let accrual_ids = value.get("accrualIds")
                .map(|v| v.to_string())
                .unwrap_or_else(|| "[]".to_string());
            tx.execute(
                "INSERT INTO commission_payouts (
                    id, reseller_key, payout_date, amount_inr, accrual_ids,
                    raw_data, created_at, modified_at
                ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    id, js_str(value, "resellerKey"),
                    js_str(value, "payoutDate"), js_f64(value, "amountINR"),
                    accrual_ids, raw, js_str(value, "createdAt"), now_iso(),
                ],
            )?;
            count += 1;
        }
        tx.commit()?;
        Ok(count)
    }) { Ok(n) => SaveResult::ok(n), Err(e) => SaveResult::err(e) }
}

#[tauri::command]
fn storage_save_company_profile(state: tauri::State<DbState>, payload: String) -> SaveResult {
    let parsed: serde_json::Value = match serde_json::from_str(&payload) {
        Ok(v) => v, Err(e) => return SaveResult::err(format!("Invalid JSON: {e}")),
    };
    if !parsed.is_object() {
        return SaveResult::err("Expected object".into());
    }
    let value = &parsed;
    let raw = value.to_string();

    match with_writer(&state, |con| {
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
                js_str(value, "legalName"), js_str(value, "tradingName"),
                js_str(value, "gstin"), js_str(value, "stateCode"),
                js_str(value, "pan"), js_str(value, "cin"),
                js_str(value, "logoDataUrl"), js_str(value, "letterheadDataUrl"),
                js_str(value, "signatureImageDataUrl"),
                js_bool_int(value, "letterheadUseAsBackground"),
                js_str(value, "invoicePrefix"), js_str(value, "proposalPrefix"),
                js_str(value, "poPrefix"),
                js_i64(value, "invoiceNextNumber"),
                js_i64(value, "proposalNextNumber"),
                js_i64(value, "poNextNumber"),
                js_str(value, "numberingMode"),
                js_str(value, "bankName"), js_str(value, "bankBranch"),
                js_str(value, "bankAccountHolder"), js_str(value, "bankAccountNumber"),
                js_str(value, "bankIfsc"),
                js_str(value, "signatoryName"), js_str(value, "signatoryDesignation"),
                raw, now_iso(),
            ],
        )?;
        Ok(1usize)
    }) { Ok(n) => SaveResult::ok(n), Err(e) => SaveResult::err(e) }
}

#[tauri::command]
fn storage_save_attachments(state: tauri::State<DbState>, payload: String) -> SaveResult {
    let parsed: serde_json::Value = match serde_json::from_str(&payload) {
        Ok(v) => v, Err(e) => return SaveResult::err(format!("Invalid JSON: {e}")),
    };
    let arr = match parsed.as_array() {
        Some(a) => a, None => return SaveResult::err("Expected array".into()),
    };
    match with_writer(&state, |con| {
        let tx = con.transaction()?;
        tx.execute("DELETE FROM attachments", [])?;
        let mut count = 0usize;
        for value in arr {
            let raw = value.to_string();
            let id = match js_str(value, "id") {
                Some(s) if !s.is_empty() => s.to_string(), _ => continue,
            };
            tx.execute(
                "INSERT INTO attachments (
                    id, related_entity_type, related_entity_id, filename, mime_type,
                    size_bytes, file_path, fallback_url, uploaded_at, raw_data, modified_at
                ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                params![
                    id, js_str(value, "relatedEntityType"),
                    js_str(value, "relatedEntityId"), js_str(value, "filename"),
                    js_str(value, "mimeType"), js_i64(value, "sizeBytes"),
                    js_str(value, "filePath"), js_str(value, "fallbackUrl"),
                    js_str(value, "uploadedAt"), raw, now_iso(),
                ],
            )?;
            count += 1;
        }
        tx.commit()?;
        Ok(count)
    }) { Ok(n) => SaveResult::ok(n), Err(e) => SaveResult::err(e) }
}

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
    }) { Ok(n) => SaveResult::ok(n), Err(e) => SaveResult::err(e) }
}

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
    state.reset();
    InstallDbResult {
        ok: true,
        error: None,
        db_path: Some(target.to_string_lossy().to_string()),
    }
}

// ============================================================================
// Session 5 — external URL opener
//
// JS intercepts OneDrive link clicks (and any other external URLs) and routes
// them through this command. The opener plugin opens the URL in the user's
// default browser, regardless of WebKit's broken target=_blank handling.
// ============================================================================

#[tauri::command]
fn open_external_url<R: tauri::Runtime>(app: tauri::AppHandle<R>, url: String) -> Result<(), String> {
    // Light validation — only allow http/https. Refuses file:// and other
    // schemes since this command is reachable from page JS.
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(format!("Refusing to open URL with non-http(s) scheme: {url}"));
    }
    app.opener().open_url(url, None::<&str>).map_err(|e| format!("{e}"))?;
    Ok(())
}

// ============================================================================
// Session 5 — reveal OneDrive folder in Finder
// ============================================================================

#[derive(Serialize)]
struct RevealResult {
    ok: bool,
    error: Option<String>,
    path: Option<String>,
}

#[tauri::command]
fn reveal_onedrive_folder<R: tauri::Runtime>(app: tauri::AppHandle<R>) -> RevealResult {
    let dir = match snapshot_dir() {
        Some(d) => d,
        None => return RevealResult {
            ok: false,
            error: Some("OneDrive folder not found at ~/OneDrive. Is OneDrive installed?".into()),
            path: None,
        },
    };
    // Create the Masterstone subfolder if it doesn't yet exist so the reveal
    // doesn't fail on a fresh install.
    let _ = std::fs::create_dir_all(&dir);
    let path_str = dir.to_string_lossy().to_string();
    match app.opener().open_path(path_str.clone(), None::<&str>) {
        Ok(_) => RevealResult { ok: true, error: None, path: Some(path_str) },
        Err(e) => RevealResult { ok: false, error: Some(format!("{e}")), path: Some(path_str) },
    }
}

// ============================================================================
// Session 5 — iPhone HTML snapshot generator
//
// Reads from SQLite, produces a self-contained HTML file with embedded data,
// writes to ~/OneDrive/Masterstone/Masterstone_Snapshot_YYYY-MM-DD.html.
// Keeps last 7 by date in filename, auto-purges older.
//
// The HTML file uses inline CSS and JS only — no external requests. iOS Safari
// renders it as a static page.
// ============================================================================

#[derive(Serialize)]
struct SnapshotResult {
    ok: bool,
    skipped: bool,
    skip_reason: Option<String>,
    error: Option<String>,
    path: Option<String>,
    bytes_written: Option<u64>,
    purged_count: usize,
}

/// Read modification time of a file, for change detection.
fn file_mtime_secs(path: &PathBuf) -> Option<u64> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?;
    let dur = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(dur.as_secs())
}

#[tauri::command]
fn get_snapshot_status() -> serde_json::Value {
    // Returns the current state of snapshot capability — used by the JS side
    // to render the manual button and "last snapshot" info.
    let dir_opt = snapshot_dir();
    let onedrive_available = dir_opt.is_some();
    let dir_path = dir_opt.as_ref().map(|d| d.to_string_lossy().to_string());

    // Find the most recent snapshot file in the folder (if any)
    let last_snapshot = dir_opt.as_ref().and_then(|dir| {
        let entries = std::fs::read_dir(dir).ok()?;
        let mut best: Option<(PathBuf, u64)> = None;
        for entry in entries.flatten() {
            let p = entry.path();
            if let Some(name) = p.file_name().and_then(|s| s.to_str()) {
                if name.starts_with("Masterstone_Snapshot_") && name.ends_with(".html") {
                    if let Some(mtime) = file_mtime_secs(&p) {
                        if best.as_ref().map(|(_, t)| mtime > *t).unwrap_or(true) {
                            best = Some((p, mtime));
                        }
                    }
                }
            }
        }
        best.map(|(p, t)| (p.to_string_lossy().to_string(), t))
    });

    let db_mtime = db_path().and_then(|p| file_mtime_secs(&p));

    serde_json::json!({
        "onedrive_available": onedrive_available,
        "snapshot_dir": dir_path,
        "last_snapshot_path": last_snapshot.as_ref().map(|(p, _)| p.clone()),
        "last_snapshot_mtime": last_snapshot.as_ref().map(|(_, t)| *t),
        "db_mtime": db_mtime,
    })
}

#[tauri::command]
fn generate_snapshot(force: Option<bool>) -> SnapshotResult {
    let force = force.unwrap_or(false);

    // 1. Resolve OneDrive folder
    let dir = match snapshot_dir() {
        Some(d) => d,
        None => return SnapshotResult {
            ok: false, skipped: false, skip_reason: None,
            error: Some("OneDrive folder not found at ~/OneDrive. Snapshot skipped.".into()),
            path: None, bytes_written: None, purged_count: 0,
        },
    };

    if let Err(e) = std::fs::create_dir_all(&dir) {
        return SnapshotResult {
            ok: false, skipped: false, skip_reason: None,
            error: Some(format!("Could not create snapshot folder: {e}")),
            path: None, bytes_written: None, purged_count: 0,
        };
    }

    // 2. Check if data has changed since last snapshot (unless forced)
    let dbp = match db_path() {
        Some(p) => p,
        None => return SnapshotResult {
            ok: false, skipped: false, skip_reason: None,
            error: Some("Could not resolve database path".into()),
            path: None, bytes_written: None, purged_count: 0,
        },
    };
    let db_mtime = file_mtime_secs(&dbp).unwrap_or(0);

    if !force {
        // Find most recent snapshot
        let mut latest_snapshot_mtime: u64 = 0;
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if let Some(name) = p.file_name().and_then(|s| s.to_str()) {
                    if name.starts_with("Masterstone_Snapshot_") && name.ends_with(".html") {
                        if let Some(t) = file_mtime_secs(&p) {
                            if t > latest_snapshot_mtime { latest_snapshot_mtime = t; }
                        }
                    }
                }
            }
        }
        if latest_snapshot_mtime > 0 && db_mtime <= latest_snapshot_mtime {
            return SnapshotResult {
                ok: true, skipped: true,
                skip_reason: Some("No changes since last snapshot.".into()),
                error: None, path: None, bytes_written: None, purged_count: 0,
            };
        }
    }

    // 3. Read all data from SQLite (reuse the read path)
    let load = match load_all_inner(&dbp) {
        Ok(r) => r,
        Err(e) => return SnapshotResult {
            ok: false, skipped: false, skip_reason: None,
            error: Some(format!("Could not read database: {e}")),
            path: None, bytes_written: None, purged_count: 0,
        },
    };

    // 4. Render HTML
    let html = render_snapshot_html(&load);

    // 5. Write file with today's date
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let filename = format!("Masterstone_Snapshot_{date}.html");
    let out_path = dir.join(&filename);
    let bytes = html.len() as u64;
    if let Err(e) = std::fs::write(&out_path, html.as_bytes()) {
        return SnapshotResult {
            ok: false, skipped: false, skip_reason: None,
            error: Some(format!("Could not write snapshot file: {e}")),
            path: Some(out_path.to_string_lossy().to_string()),
            bytes_written: None, purged_count: 0,
        };
    }

    // 6. Purge older snapshots — keep last 7
    let purged = purge_old_snapshots(&dir, 7);

    SnapshotResult {
        ok: true, skipped: false, skip_reason: None,
        error: None,
        path: Some(out_path.to_string_lossy().to_string()),
        bytes_written: Some(bytes),
        purged_count: purged,
    }
}

/// Keep the `keep` most recent Masterstone_Snapshot_*.html files, delete others.
fn purge_old_snapshots(dir: &PathBuf, keep: usize) -> usize {
    let mut snapshots: Vec<(PathBuf, u64)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if let Some(name) = p.file_name().and_then(|s| s.to_str()) {
                if name.starts_with("Masterstone_Snapshot_") && name.ends_with(".html") {
                    if let Some(t) = file_mtime_secs(&p) {
                        snapshots.push((p, t));
                    }
                }
            }
        }
    }
    if snapshots.len() <= keep {
        return 0;
    }
    snapshots.sort_by(|a, b| b.1.cmp(&a.1)); // newest first
    let to_delete = &snapshots[keep..];
    let mut purged = 0;
    for (path, _) in to_delete {
        if std::fs::remove_file(path).is_ok() {
            purged += 1;
        }
    }
    purged
}

// ============================================================================
// HTML snapshot rendering
//
// Strategy: produce a self-contained HTML file with all data embedded as a
// JSON blob in a <script> tag, plus inline JS that renders simple read-only
// views. iOS Safari handles ~5MB HTML files comfortably.
//
// Per Decision 3C: includes dashboards, client list, reseller list,
// contracts, latest 50 invoices, active prospects. No edit forms, no PDF gen.
// ============================================================================

fn render_snapshot_html(data: &LoadAllResult) -> String {
    // Bundle data as JSON for embedding. We use the localStorage-shaped data
    // from the read path so the snapshot's JS can use the same access patterns.
    let payload = serde_json::json!({
        "generated_at": chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        "record_counts": data.record_counts,
        "data": data.data,
    });
    // Escape </script> just in case any data contains it
    let payload_str = payload.to_string().replace("</", "<\\/");

    format!(r###"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1, maximum-scale=2">
<meta name="apple-mobile-web-app-capable" content="yes">
<title>Masterstone Snapshot</title>
<style>
*{{box-sizing:border-box;margin:0;padding:0;-webkit-text-size-adjust:100%;}}
body{{font-family:-apple-system,BlinkMacSystemFont,"SF Pro Text","Segoe UI",sans-serif;background:#f6f7fa;color:#1a1f2e;font-size:15px;line-height:1.45;padding:0 0 80px 0;}}
header{{background:linear-gradient(135deg,#4f46e5,#6366f1);color:#fff;padding:20px 18px 16px;position:sticky;top:0;z-index:50;box-shadow:0 2px 8px rgba(0,0,0,.12);}}
header h1{{font-size:18px;font-weight:600;letter-spacing:.01em;}}
header .subtitle{{font-size:12px;opacity:.85;margin-top:3px;}}
nav.tabs{{display:flex;overflow-x:auto;background:#fff;border-bottom:1px solid #e5e7eb;position:sticky;top:64px;z-index:49;}}
nav.tabs button{{flex:0 0 auto;padding:14px 16px;background:none;border:none;font-size:14px;font-weight:500;color:#6b7280;cursor:pointer;font-family:inherit;border-bottom:2px solid transparent;}}
nav.tabs button.active{{color:#4f46e5;border-bottom-color:#4f46e5;}}
main{{padding:14px;}}
.summary-grid{{display:grid;grid-template-columns:1fr 1fr;gap:10px;margin-bottom:18px;}}
.summary-card{{background:#fff;border-radius:10px;padding:14px;box-shadow:0 1px 2px rgba(0,0,0,.05);border:1px solid #eef0f4;}}
.summary-card .label{{font-size:11px;text-transform:uppercase;letter-spacing:.06em;color:#6b7280;font-weight:500;}}
.summary-card .value{{font-size:22px;font-weight:600;color:#1a1f2e;margin-top:4px;}}
.summary-card .sublabel{{font-size:11px;color:#9ca3af;margin-top:2px;}}
section{{display:none;}}
section.active{{display:block;}}
section h2{{font-size:16px;font-weight:600;margin-bottom:12px;color:#1a1f2e;}}
.card{{background:#fff;border-radius:10px;padding:14px 16px;margin-bottom:8px;box-shadow:0 1px 2px rgba(0,0,0,.04);border:1px solid #eef0f4;}}
.card .top-row{{display:flex;justify-content:space-between;align-items:flex-start;margin-bottom:6px;}}
.card .name{{font-size:15px;font-weight:600;color:#1a1f2e;}}
.card .meta{{font-size:12px;color:#6b7280;margin-top:2px;}}
.card .badge{{display:inline-block;padding:2px 8px;border-radius:10px;font-size:11px;font-weight:500;}}
.badge-active{{background:#dcfce7;color:#166534;}}
.badge-renewing{{background:#fef3c7;color:#92400e;}}
.badge-expired{{background:#fee2e2;color:#991b1b;}}
.badge-paid{{background:#dcfce7;color:#166534;}}
.badge-due{{background:#fef3c7;color:#92400e;}}
.badge-overdue{{background:#fee2e2;color:#991b1b;}}
.badge-default{{background:#e5e7eb;color:#374151;}}
.amt{{font-variant-numeric:tabular-nums;font-weight:600;color:#1a1f2e;}}
.empty{{text-align:center;color:#9ca3af;font-size:13px;padding:36px 12px;background:#fff;border-radius:10px;border:1px dashed #d1d5db;}}
.search-box{{width:100%;padding:11px 14px;font-size:15px;border:1px solid #d1d5db;border-radius:9px;margin-bottom:12px;font-family:inherit;background:#fff;-webkit-appearance:none;}}
.search-box:focus{{outline:none;border-color:#4f46e5;box-shadow:0 0 0 3px rgba(79,70,229,.12);}}
footer{{position:fixed;bottom:0;left:0;right:0;text-align:center;padding:8px;background:rgba(255,255,255,.95);font-size:11px;color:#6b7280;border-top:1px solid #e5e7eb;backdrop-filter:blur(8px);}}
@media (min-width: 600px){{
.summary-grid{{grid-template-columns:repeat(4,1fr);}}
main{{padding:20px 28px;max-width:900px;margin:0 auto;}}
}}
</style>
</head>
<body>
<header>
<h1>📊 Masterstone CRM Snapshot</h1>
<div class="subtitle">Generated <span id="genTime"></span> · Read-only mobile view</div>
</header>
<nav class="tabs">
<button class="active" data-section="dashboard">Overview</button>
<button data-section="clients">Clients</button>
<button data-section="contracts">Contracts</button>
<button data-section="invoices">Invoices</button>
<button data-section="prospects">Prospects</button>
<button data-section="resellers">Resellers</button>
</nav>
<main>
<section id="sec-dashboard" class="active"><div class="summary-grid" id="dashGrid"></div><div id="dashExtras"></div></section>
<section id="sec-clients"><h2>Clients</h2><input class="search-box" placeholder="Search clients…" oninput="filterCards('clients',this.value)"><div id="clientsList"></div></section>
<section id="sec-contracts"><h2>Contracts</h2><input class="search-box" placeholder="Search contracts…" oninput="filterCards('contracts',this.value)"><div id="contractsList"></div></section>
<section id="sec-invoices"><h2>Recent Invoices</h2><input class="search-box" placeholder="Search invoices…" oninput="filterCards('invoices',this.value)"><div id="invoicesList"></div></section>
<section id="sec-prospects"><h2>Active Prospects</h2><input class="search-box" placeholder="Search prospects…" oninput="filterCards('prospects',this.value)"><div id="prospectsList"></div></section>
<section id="sec-resellers"><h2>Resellers / Partners</h2><input class="search-box" placeholder="Search resellers…" oninput="filterCards('resellers',this.value)"><div id="resellersList"></div></section>
</main>
<footer>Self-contained snapshot · No live data · Use Mac app for edits</footer>

<script id="ms-data" type="application/json">{payload}</script>
<script>
(function(){{
var raw = document.getElementById('ms-data').textContent;
var pkg = JSON.parse(raw);
var data = {{}};
Object.keys(pkg.data).forEach(function(k){{
  try {{ data[k] = JSON.parse(pkg.data[k]); }}
  catch (e) {{ data[k] = pkg.data[k]; }}
}});

document.getElementById('genTime').textContent = pkg.generated_at;

var contracts = data.ms_pro_v210 || [];
var clients   = data.ms_client_master_v1 || {{}};
var resellers = data.ms_reseller_master_v1 || {{}};
var invoices  = data.ms_invoices_v1 || [];
var prospects = data.ms_prospects_v1 || [];

// Tabs
document.querySelectorAll('nav.tabs button').forEach(function(btn){{
  btn.addEventListener('click', function(){{
    document.querySelectorAll('nav.tabs button').forEach(function(b){{b.classList.remove('active');}});
    document.querySelectorAll('section').forEach(function(s){{s.classList.remove('active');}});
    btn.classList.add('active');
    var sec = document.getElementById('sec-' + btn.dataset.section);
    if (sec) sec.classList.add('active');
    window.scrollTo(0, 0);
  }});
}});

// Dashboard
var totalClients = Object.keys(clients).length;
var totalContracts = contracts.length;
var activeContracts = contracts.filter(function(c){{ return c.renewalStatus !== 'Expired' && c.renewalStatus !== 'Lost'; }}).length;
var totalInvoices = invoices.length;
var totalRevenue = invoices.reduce(function(sum, i){{ return sum + (parseFloat(i.grandTotal) || 0); }}, 0);
var outstandingAmount = invoices.reduce(function(sum, i){{
  var status = (i.status || '').toLowerCase();
  if (status === 'paid' || status === 'cancelled') return sum;
  return sum + (parseFloat(i.amountOutstanding) || parseFloat(i.grandTotal) || 0);
}}, 0);

function fmtINR(n){{
  if (!isFinite(n)) return '—';
  if (n >= 1e7) return '₹' + (n/1e7).toFixed(2) + ' Cr';
  if (n >= 1e5) return '₹' + (n/1e5).toFixed(2) + ' L';
  return '₹' + Math.round(n).toLocaleString('en-IN');
}}

document.getElementById('dashGrid').innerHTML =
  '<div class="summary-card"><div class="label">Clients</div><div class="value">'+ totalClients +'</div></div>' +
  '<div class="summary-card"><div class="label">Contracts</div><div class="value">'+ totalContracts +'</div><div class="sublabel">'+ activeContracts +' active</div></div>' +
  '<div class="summary-card"><div class="label">Invoices</div><div class="value">'+ totalInvoices +'</div></div>' +
  '<div class="summary-card"><div class="label">Outstanding</div><div class="value">'+ fmtINR(outstandingAmount) +'</div></div>';

// Clients list
function clientCard(name, c){{
  var contracts_count = contracts.filter(function(x){{ return (x.client || x.clientName) === name; }}).length;
  var industry = c.industry || '—';
  var status = c.status || '';
  var sBadge = status === 'Active' ? '<span class="badge badge-active">Active</span>' : (status ? '<span class="badge badge-default">'+ esc(status) +'</span>' : '');
  return '<div class="card" data-search="'+ esc((name + ' ' + industry).toLowerCase()) +'">' +
    '<div class="top-row"><div><div class="name">'+ esc(name) +'</div><div class="meta">'+ esc(industry) +' · '+ contracts_count +' contracts</div></div>' +
    sBadge + '</div></div>';
}}
function esc(s){{ return String(s == null ? '' : s).replace(/[&<>"]/g, function(c){{ return {{'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}}[c]; }}); }}

var clientCards = Object.keys(clients).sort().map(function(n){{ return clientCard(n, clients[n]); }}).join('');
document.getElementById('clientsList').innerHTML = clientCards || '<div class="empty">No clients.</div>';

// Contracts
function contractCard(c){{
  var client = c.client || c.clientName || '';
  var product = c.product || '';
  var sell = parseFloat(c.sellINR) || 0;
  var status = c.renewalStatus || '';
  var bClass = 'badge-default';
  if (status === 'Active') bClass = 'badge-active';
  else if (status === 'Renewing' || status === 'Up for Renewal') bClass = 'badge-renewing';
  else if (status === 'Expired' || status === 'Lost') bClass = 'badge-expired';
  return '<div class="card" data-search="'+ esc((client + ' ' + product).toLowerCase()) +'">' +
    '<div class="top-row"><div><div class="name">'+ esc(product) +'</div><div class="meta">'+ esc(client) +'</div></div>' +
    (status ? '<span class="badge '+ bClass +'">'+ esc(status) +'</span>' : '') + '</div>' +
    '<div class="meta">'+ (c.startDate || '') +' → '+ (c.endDate || '') +' · <span class="amt">'+ fmtINR(sell) +'</span></div></div>';
}}
var contractCards = contracts.slice().sort(function(a,b){{
  return (b.endDate || '').localeCompare(a.endDate || '');
}}).map(contractCard).join('');
document.getElementById('contractsList').innerHTML = contractCards || '<div class="empty">No contracts.</div>';

// Invoices (latest 50)
function invoiceCard(i){{
  var num = i.invoiceNumber || i.id || '';
  var client = i.clientName || '';
  var amt = parseFloat(i.grandTotal) || 0;
  var status = i.status || '';
  var bClass = 'badge-default';
  var s = status.toLowerCase();
  if (s === 'paid') bClass = 'badge-paid';
  else if (s === 'overdue') bClass = 'badge-overdue';
  else if (s === 'due' || s === 'sent' || s === 'issued') bClass = 'badge-due';
  return '<div class="card" data-search="'+ esc((num + ' ' + client).toLowerCase()) +'">' +
    '<div class="top-row"><div><div class="name">'+ esc(num) +'</div><div class="meta">'+ esc(client) +'</div></div>' +
    (status ? '<span class="badge '+ bClass +'">'+ esc(status) +'</span>' : '') + '</div>' +
    '<div class="meta">'+ (i.invoiceDate || '') +' · <span class="amt">'+ fmtINR(amt) +'</span></div></div>';
}}
var sortedInv = invoices.slice().sort(function(a,b){{
  return (b.invoiceDate || '').localeCompare(a.invoiceDate || '');
}}).slice(0, 50);
var invoiceCards = sortedInv.map(invoiceCard).join('');
document.getElementById('invoicesList').innerHTML = invoiceCards || '<div class="empty">No invoices.</div>';

// Prospects (active only — not archived, stage not Closed-Lost)
function prospectCard(p){{
  var company = p.company || '';
  var opp = p.oppName || '';
  var stage = p.stage || '';
  var acv = parseFloat(p.acv) || 0;
  return '<div class="card" data-search="'+ esc((company + ' ' + opp).toLowerCase()) +'">' +
    '<div class="top-row"><div><div class="name">'+ esc(opp || company) +'</div><div class="meta">'+ esc(company) +'</div></div>' +
    (stage ? '<span class="badge badge-default">'+ esc(stage) +'</span>' : '') + '</div>' +
    '<div class="meta">'+ (p.closeDate || '') +' · <span class="amt">'+ fmtINR(acv) +'</span></div></div>';
}}
var activeProspects = prospects.filter(function(p){{
  if (p.archived) return false;
  var stage = (p.stage || '').toLowerCase();
  return stage !== 'closed lost' && stage !== 'closed-lost' && stage !== 'lost';
}});
var prospectCards = activeProspects.map(prospectCard).join('');
document.getElementById('prospectsList').innerHTML = prospectCards || '<div class="empty">No active prospects.</div>';

// Resellers
function resellerCard(name, r){{
  var status = r.status || '';
  return '<div class="card" data-search="'+ esc(name.toLowerCase()) +'">' +
    '<div class="top-row"><div><div class="name">'+ esc(name) +'</div><div class="meta">'+ esc(r.shortName || '') +'</div></div>' +
    (status === 'Active' ? '<span class="badge badge-active">Active</span>' : (status ? '<span class="badge badge-default">'+ esc(status) +'</span>' : '')) + '</div></div>';
}}
var resellerCards = Object.keys(resellers).sort().map(function(n){{ return resellerCard(n, resellers[n]); }}).join('');
document.getElementById('resellersList').innerHTML = resellerCards || '<div class="empty">No resellers.</div>';

// Search filter
window.filterCards = function(section, query){{
  var q = (query || '').trim().toLowerCase();
  var listIds = {{
    clients: 'clientsList', contracts: 'contractsList', invoices: 'invoicesList',
    prospects: 'prospectsList', resellers: 'resellersList'
  }};
  var container = document.getElementById(listIds[section]);
  if (!container) return;
  Array.prototype.forEach.call(container.children, function(card){{
    if (!card.dataset.search) return;
    card.style.display = card.dataset.search.indexOf(q) !== -1 ? '' : 'none';
  }});
}};
}})();
</script>
</body>
</html>"###, payload = payload_str)
}

// ============================================================================
// Entry point
// ============================================================================

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
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
            open_external_url,
            reveal_onedrive_folder,
            generate_snapshot,
            get_snapshot_status,
        ])
        .setup(|_app| Ok(()))
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
