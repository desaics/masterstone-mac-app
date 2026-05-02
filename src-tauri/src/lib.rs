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
    // Session 6: track mtime of the .db file at the time we opened it.
    // If a write detects the file's mtime has changed without us doing it,
    // an external process (typically migrate.py) modified the file —
    // surfaced as CONFLICT to the JS side which prompts the user.
    last_known_mtime: Mutex<Option<u64>>,
}

impl DbState {
    fn new() -> Self {
        Self {
            conn: Mutex::new(None),
            last_known_mtime: Mutex::new(None),
        }
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
        // Capture mtime for the conflict guard.
        if let Ok(mut mt) = self.last_known_mtime.lock() {
            *mt = file_mtime_secs(&path);
        }
        Ok(())
    }

    fn reset(&self) {
        if let Ok(mut guard) = self.conn.lock() {
            *guard = None;
        }
        if let Ok(mut mt) = self.last_known_mtime.lock() {
            *mt = None;
        }
    }

    /// Returns Some(true) if the file's mtime changed since we last recorded it.
    /// Returns Some(false) if it hasn't changed. Returns None if we can't tell
    /// (no recorded mtime yet, or filesystem error).
    fn detect_external_change(&self) -> Option<bool> {
        let recorded = self.last_known_mtime.lock().ok()?.clone()?;
        let current = file_mtime_secs(&db_path()?)?;
        // Allow ±1 second wiggle room — different filesystems have different
        // mtime resolutions (HFS is whole seconds, APFS is sub-second).
        Some(current > recorded + 1)
    }

    /// Refresh the recorded mtime — call after a successful write so the next
    /// write's conflict check compares against the new state.
    fn refresh_mtime(&self) {
        if let Ok(mut mt) = self.last_known_mtime.lock() {
            if let Some(p) = db_path() {
                *mt = file_mtime_secs(&p);
            }
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
    let result = f(con).map_err(|e| format!("{e}"))?;
    // After every successful write, refresh the recorded mtime so
    // subsequent writes don't trip the conflict guard on our own work.
    drop(guard);
    state.refresh_mtime();
    Ok(result)
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
// Bug fix #50 (Session 5 hotfix v2) — original template embedded data as a
// JSON blob and rendered it client-side with JavaScript. iOS Quick Look
// (which is what iOS Files and the OneDrive iOS app use to preview HTML
// files) does NOT execute JavaScript, so users on iPhone saw only the
// header and empty sections.
//
// New strategy: render everything server-side in Rust at generation time.
// Output is fully static HTML with no <script> tags. All sections are
// always visible in a single scrollable page; the tab nav at top provides
// quick-jump anchor links to each section. This avoids any CSS dependency
// on :target or :has() which could behave inconsistently across WebKit
// versions (especially the stripped-down WebKit used by iOS Quick Look).
// File is ~2-3x bigger than the JSON-blob version but renders identically
// everywhere — Quick Look, mobile Safari, desktop browsers.
//
// Per Decision 3C: dashboards, client list, reseller list, contracts,
// latest 50 invoices, active prospects. No edit forms, no PDF generators.
// ============================================================================

/// HTML-escape a value for safe insertion into HTML body or attributes.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&'  => out.push_str("&amp;"),
            '<'  => out.push_str("&lt;"),
            '>'  => out.push_str("&gt;"),
            '"'  => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _    => out.push(c),
        }
    }
    out
}

/// Render an INR amount with the same fmtINR semantics as the previous JS:
/// Crore for ≥1e7, Lakh for ≥1e5, plain rupees otherwise. Returns "—" for
/// non-finite values.
fn fmt_inr(n: f64) -> String {
    if !n.is_finite() {
        return "—".to_string();
    }
    if n.abs() >= 1e7 {
        return format!("₹{:.2} Cr", n / 1e7);
    }
    if n.abs() >= 1e5 {
        return format!("₹{:.2} L", n / 1e5);
    }
    // Indian-grouped integer rendering for amounts under 1 Lakh.
    let rounded = n.round() as i64;
    let s = rounded.abs().to_string();
    let mut out = String::new();
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    if len <= 3 {
        out.push_str(&s);
    } else {
        let last3 = &s[len - 3..];
        let rest = &s[..len - 3];
        let rest_chars: Vec<char> = rest.chars().rev().collect();
        let mut grouped = String::new();
        for (i, c) in rest_chars.iter().enumerate() {
            if i > 0 && i % 2 == 0 { grouped.push(','); }
            grouped.push(*c);
        }
        let rest_grouped: String = grouped.chars().rev().collect();
        out.push_str(&rest_grouped);
        out.push(',');
        out.push_str(last3);
    }
    if rounded < 0 { format!("-₹{out}") } else { format!("₹{out}") }
}

/// Read a string field from a JSON value, returning empty string if missing.
fn jss(v: &serde_json::Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

/// Read a numeric field, accepting numbers or numeric strings.
fn jsn(v: &serde_json::Value, key: &str) -> f64 {
    let val = match v.get(key) { Some(x) => x, None => return 0.0 };
    if let Some(n) = val.as_f64() { return n; }
    if let Some(s) = val.as_str() { return s.parse::<f64>().unwrap_or(0.0); }
    0.0
}

fn render_snapshot_html(data: &LoadAllResult) -> String {
    // Parse the localStorage-shaped strings back into structured JSON so we
    // can iterate. (The values inside data.data are JSON-string-encoded.)
    fn parse_or_null(s: &str) -> serde_json::Value {
        serde_json::from_str(s).unwrap_or(serde_json::Value::Null)
    }
    let contracts = data.data.get("ms_pro_v210")
        .map(|s| parse_or_null(s))
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    let clients = data.data.get("ms_client_master_v1")
        .map(|s| parse_or_null(s))
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    let resellers = data.data.get("ms_reseller_master_v1")
        .map(|s| parse_or_null(s))
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    let invoices = data.data.get("ms_invoices_v1")
        .map(|s| parse_or_null(s))
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    let prospects = data.data.get("ms_prospects_v1")
        .map(|s| parse_or_null(s))
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();

    let generated_at = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();

    // ---- Dashboard summary ----
    let total_clients = clients.len();
    let total_contracts = contracts.len();
    let active_contracts = contracts.iter().filter(|c| {
        let s = jss(c, "renewalStatus");
        s != "Expired" && s != "Lost"
    }).count();
    let total_invoices = invoices.len();
    let outstanding: f64 = invoices.iter().filter_map(|inv| {
        let status = jss(inv, "status").to_lowercase();
        if status == "paid" || status == "cancelled" { return None; }
        let out_amt = jsn(inv, "amountOutstanding");
        let grand = jsn(inv, "grandTotal");
        Some(if out_amt > 0.0 { out_amt } else { grand })
    }).sum();

    // ---- Build sections ----

    // Clients section
    let mut clients_html = String::new();
    let mut client_names: Vec<&String> = clients.keys().collect();
    client_names.sort();
    for name in &client_names {
        let c = &clients[*name];
        let industry = jss(c, "industry");
        let industry_disp = if industry.is_empty() { "—".to_string() } else { industry.clone() };
        let status = jss(c, "status");
        let badge = if status == "Active" {
            "<span class=\"badge badge-active\">Active</span>".to_string()
        } else if status.is_empty() {
            "".to_string()
        } else {
            format!("<span class=\"badge badge-default\">{}</span>", esc(&status))
        };
        let n_contracts = contracts.iter().filter(|x| {
            let cn = jss(x, "client");
            let cn2 = jss(x, "clientName");
            cn == **name || cn2 == **name
        }).count();
        clients_html.push_str(&format!(
            "<div class=\"card\"><div class=\"top-row\"><div><div class=\"name\">{name}</div><div class=\"meta\">{industry} · {n} contracts</div></div>{badge}</div></div>",
            name = esc(name),
            industry = esc(&industry_disp),
            n = n_contracts,
            badge = badge,
        ));
    }
    if clients_html.is_empty() {
        clients_html = "<div class=\"empty\">No clients.</div>".to_string();
    }

    // Contracts section — sorted by endDate descending
    let mut contracts_sorted: Vec<&serde_json::Value> = contracts.iter().collect();
    contracts_sorted.sort_by(|a, b| jss(b, "endDate").cmp(&jss(a, "endDate")));
    let mut contracts_html = String::new();
    for c in &contracts_sorted {
        let client = if !jss(c, "client").is_empty() { jss(c, "client") } else { jss(c, "clientName") };
        let product = jss(c, "product");
        let sell = jsn(c, "sellINR");
        let status = jss(c, "renewalStatus");
        let badge_class = match status.as_str() {
            "Active" => "badge-active",
            "Renewing" | "Up for Renewal" => "badge-renewing",
            "Expired" | "Lost" => "badge-expired",
            _ => "badge-default",
        };
        let badge = if status.is_empty() {
            "".to_string()
        } else {
            format!("<span class=\"badge {}\">{}</span>", badge_class, esc(&status))
        };
        contracts_html.push_str(&format!(
            "<div class=\"card\"><div class=\"top-row\"><div><div class=\"name\">{prod}</div><div class=\"meta\">{client}</div></div>{badge}</div><div class=\"meta\">{start} → {end} · <span class=\"amt\">{amt}</span></div></div>",
            prod = esc(&product),
            client = esc(&client),
            start = esc(&jss(c, "startDate")),
            end = esc(&jss(c, "endDate")),
            amt = fmt_inr(sell),
            badge = badge,
        ));
    }
    if contracts_html.is_empty() {
        contracts_html = "<div class=\"empty\">No contracts.</div>".to_string();
    }

    // Invoices section — latest 50, sorted by invoiceDate descending
    let mut invoices_sorted: Vec<&serde_json::Value> = invoices.iter().collect();
    invoices_sorted.sort_by(|a, b| jss(b, "invoiceDate").cmp(&jss(a, "invoiceDate")));
    let mut invoices_html = String::new();
    for inv in invoices_sorted.iter().take(50) {
        let num = if !jss(inv, "invoiceNumber").is_empty() { jss(inv, "invoiceNumber") } else { jss(inv, "id") };
        let client = jss(inv, "clientName");
        let amt = jsn(inv, "grandTotal");
        let status = jss(inv, "status");
        let s_lower = status.to_lowercase();
        let badge_class = match s_lower.as_str() {
            "paid" => "badge-paid",
            "overdue" => "badge-overdue",
            "due" | "sent" | "issued" => "badge-due",
            _ => "badge-default",
        };
        let badge = if status.is_empty() {
            "".to_string()
        } else {
            format!("<span class=\"badge {}\">{}</span>", badge_class, esc(&status))
        };
        invoices_html.push_str(&format!(
            "<div class=\"card\"><div class=\"top-row\"><div><div class=\"name\">{num}</div><div class=\"meta\">{client}</div></div>{badge}</div><div class=\"meta\">{date} · <span class=\"amt\">{amt}</span></div></div>",
            num = esc(&num),
            client = esc(&client),
            date = esc(&jss(inv, "invoiceDate")),
            amt = fmt_inr(amt),
            badge = badge,
        ));
    }
    if invoices_html.is_empty() {
        invoices_html = "<div class=\"empty\">No invoices.</div>".to_string();
    }

    // Prospects section — active only
    let mut prospects_html = String::new();
    for p in &prospects {
        let archived = p.get("archived").and_then(|v| v.as_bool()).unwrap_or(false);
        if archived { continue; }
        let stage_lower = jss(p, "stage").to_lowercase();
        if stage_lower == "closed lost" || stage_lower == "closed-lost" || stage_lower == "lost" { continue; }

        let company = jss(p, "company");
        let opp = jss(p, "oppName");
        let title = if !opp.is_empty() { opp.clone() } else { company.clone() };
        let stage = jss(p, "stage");
        let acv = jsn(p, "acv");
        let badge = if stage.is_empty() {
            "".to_string()
        } else {
            format!("<span class=\"badge badge-default\">{}</span>", esc(&stage))
        };
        prospects_html.push_str(&format!(
            "<div class=\"card\"><div class=\"top-row\"><div><div class=\"name\">{title}</div><div class=\"meta\">{company}</div></div>{badge}</div><div class=\"meta\">{date} · <span class=\"amt\">{amt}</span></div></div>",
            title = esc(&title),
            company = esc(&company),
            date = esc(&jss(p, "closeDate")),
            amt = fmt_inr(acv),
            badge = badge,
        ));
    }
    if prospects_html.is_empty() {
        prospects_html = "<div class=\"empty\">No active prospects.</div>".to_string();
    }

    // Resellers section
    let mut resellers_html = String::new();
    let mut reseller_names: Vec<&String> = resellers.keys().collect();
    reseller_names.sort();
    for name in &reseller_names {
        let r = &resellers[*name];
        let short = jss(r, "shortName");
        let status = jss(r, "status");
        let badge = if status == "Active" {
            "<span class=\"badge badge-active\">Active</span>".to_string()
        } else if status.is_empty() {
            "".to_string()
        } else {
            format!("<span class=\"badge badge-default\">{}</span>", esc(&status))
        };
        resellers_html.push_str(&format!(
            "<div class=\"card\"><div class=\"top-row\"><div><div class=\"name\">{name}</div><div class=\"meta\">{short}</div></div>{badge}</div></div>",
            name = esc(name),
            short = esc(&short),
            badge = badge,
        ));
    }
    if resellers_html.is_empty() {
        resellers_html = "<div class=\"empty\">No resellers.</div>".to_string();
    }

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
header{{background:linear-gradient(135deg,#4f46e5,#6366f1);color:#fff;padding:20px 18px 16px;}}
header h1{{font-size:18px;font-weight:600;letter-spacing:.01em;}}
header .subtitle{{font-size:12px;opacity:.85;margin-top:3px;}}

nav.tabs{{background:#fff;border-bottom:1px solid #e5e7eb;padding:12px 16px;text-align:center;}}
.counts-summary{{font-size:13px;color:#6b7280;font-weight:500;}}

main{{padding:14px;}}
.summary-grid{{display:grid;grid-template-columns:1fr 1fr;gap:10px;margin-bottom:18px;}}
.summary-card{{background:#fff;border-radius:10px;padding:14px;box-shadow:0 1px 2px rgba(0,0,0,.05);border:1px solid #eef0f4;}}
.summary-card .label{{font-size:11px;text-transform:uppercase;letter-spacing:.06em;color:#6b7280;font-weight:500;}}
.summary-card .value{{font-size:22px;font-weight:600;color:#1a1f2e;margin-top:4px;}}
.summary-card .sublabel{{font-size:11px;color:#9ca3af;margin-top:2px;}}

/* All sections always visible — tabs are quick-jump anchors via fragment IDs.
   Avoids :target / :has() CSS quirks across different WebKit versions
   (especially iOS Quick Look which uses a stripped-down WebKit). */
section{{margin-bottom:32px;scroll-margin-top:60px;}}
section h2{{font-size:18px;font-weight:600;margin-bottom:12px;color:#1a1f2e;padding-top:8px;border-top:2px solid #e5e7eb;}}
section#sec-dashboard h2{{display:none;}}
.card{{background:#fff;border-radius:10px;padding:14px 16px;margin-bottom:8px;box-shadow:0 1px 2px rgba(0,0,0,.04);border:1px solid #eef0f4;}}
.card .top-row{{display:flex;justify-content:space-between;align-items:flex-start;margin-bottom:6px;}}
.card .name{{font-size:15px;font-weight:600;color:#1a1f2e;}}
.card .meta{{font-size:12px;color:#6b7280;margin-top:2px;}}
.card .badge{{display:inline-block;padding:2px 8px;border-radius:10px;font-size:11px;font-weight:500;flex-shrink:0;margin-left:8px;}}
.badge-active{{background:#dcfce7;color:#166534;}}
.badge-renewing{{background:#fef3c7;color:#92400e;}}
.badge-expired{{background:#fee2e2;color:#991b1b;}}
.badge-paid{{background:#dcfce7;color:#166534;}}
.badge-due{{background:#fef3c7;color:#92400e;}}
.badge-overdue{{background:#fee2e2;color:#991b1b;}}
.badge-default{{background:#e5e7eb;color:#374151;}}
.amt{{font-variant-numeric:tabular-nums;font-weight:600;color:#1a1f2e;}}
.empty{{text-align:center;color:#9ca3af;font-size:13px;padding:36px 12px;background:#fff;border-radius:10px;border:1px dashed #d1d5db;}}
footer{{text-align:center;padding:16px 8px;font-size:11px;color:#6b7280;border-top:1px solid #e5e7eb;margin-top:24px;}}
@media (min-width: 600px){{
.summary-grid{{grid-template-columns:repeat(4,1fr);}}
main{{padding:20px 28px;max-width:900px;margin:0 auto;}}
}}
</style>
</head>
<body>
<header>
<h1>📊 Masterstone CRM Snapshot</h1>
<div class="subtitle">Generated {generated_at} · Read-only mobile view</div>
</header>
<nav class="tabs">
<span class="counts-summary">{total_clients} Clients · {total_contracts} Contracts · {total_invoices} Invoices · scroll down to view</span>
</nav>
<main>

<section id="sec-dashboard">
<div class="summary-grid">
<div class="summary-card"><div class="label">Clients</div><div class="value">{total_clients}</div></div>
<div class="summary-card"><div class="label">Contracts</div><div class="value">{total_contracts}</div><div class="sublabel">{active_contracts} active</div></div>
<div class="summary-card"><div class="label">Invoices</div><div class="value">{total_invoices}</div></div>
<div class="summary-card"><div class="label">Outstanding</div><div class="value">{outstanding}</div></div>
</div>
</section>

<section id="sec-clients">
<h2>Clients ({total_clients})</h2>
{clients_html}
</section>

<section id="sec-contracts">
<h2>Contracts ({total_contracts})</h2>
{contracts_html}
</section>

<section id="sec-invoices">
<h2>Recent Invoices (latest 50)</h2>
{invoices_html}
</section>

<section id="sec-prospects">
<h2>Active Prospects</h2>
{prospects_html}
</section>

<section id="sec-resellers">
<h2>Resellers / Partners</h2>
{resellers_html}
</section>

</main>
<footer>Self-contained snapshot · No live data · Use Mac app for edits<br>📱 Tap a tab above to jump to a section, or scroll through</footer>
</body>
</html>"###,
        generated_at = esc(&generated_at),
        total_clients = total_clients,
        total_contracts = total_contracts,
        active_contracts = active_contracts,
        total_invoices = total_invoices,
        outstanding = fmt_inr(outstanding),
        clients_html = clients_html,
        contracts_html = contracts_html,
        invoices_html = invoices_html,
        prospects_html = prospects_html,
        resellers_html = resellers_html,
    )
}

// ============================================================================
// Session 6 — settings management
//
// Settings are stored in the existing app_settings table (key-value).
// Different from per-bucket storage_save_setting in Session 4 because Session 4
// handled CRM-internal localStorage settings (theme, view prefs); Session 6
// handles Mac-app-only configuration the CRM doesn't know about.
//
// Key namespace (all string-valued in SQLite, JS interprets shapes):
//   ms_app__snapshot_folder         — string path or "" for default
//   ms_app__snapshot_retention      — integer string, default "7"
//   ms_app__backup_folder           — string path or "" for default
//   ms_app__backup_retention        — integer string, default "14"
//   ms_app__backup_destination_mode — "local" (default) or "onedrive"
//   ms_app__conflict_guard_enabled  — "1" (default) or "0"
//   ms_app__production_announcement_dismissed — "1" once dismissed, else absent
// ============================================================================

#[tauri::command]
fn get_app_settings() -> serde_json::Value {
    let path = match db_path() {
        Some(p) => p,
        None => return serde_json::json!({"ok": false, "error": "no db path"}),
    };
    if !path.exists() {
        return serde_json::json!({"ok": false, "error": "DB_NOT_FOUND"});
    }
    let con = match Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(c) => c,
        Err(e) => return serde_json::json!({"ok": false, "error": format!("open: {e}")}),
    };
    let mut out = serde_json::Map::new();
    let mut stmt = match con.prepare("SELECT key, value FROM app_settings WHERE key LIKE 'ms_app__%'") {
        Ok(s) => s,
        Err(e) => return serde_json::json!({"ok": false, "error": format!("prepare: {e}")}),
    };
    let rows = match stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))) {
        Ok(r) => r,
        Err(e) => return serde_json::json!({"ok": false, "error": format!("query: {e}")}),
    };
    for row in rows {
        if let Ok((k, v)) = row {
            out.insert(k, serde_json::Value::String(v));
        }
    }
    serde_json::json!({"ok": true, "settings": out})
}

#[tauri::command]
fn save_app_setting(state: tauri::State<DbState>, key: String, value: String) -> SaveResult {
    if !key.starts_with("ms_app__") {
        return SaveResult::err(format!("Invalid app setting key: {key}"));
    }
    match with_writer(&state, |con| {
        con.execute(
            "INSERT INTO app_settings (key, value, modified_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, modified_at = excluded.modified_at",
            params![key, value, now_iso()],
        )?;
        Ok(1usize)
    }) {
        Ok(n) => SaveResult::ok(n),
        Err(e) => SaveResult::err(e),
    }
}

#[tauri::command]
async fn pick_folder<R: tauri::Runtime>(app: tauri::AppHandle<R>, title: Option<String>) -> serde_json::Value {
    use tauri_plugin_dialog::DialogExt;
    let mut builder = app.dialog().file();
    if let Some(t) = title.as_ref() {
        builder = builder.set_title(t);
    }
    match builder.blocking_pick_folder() {
        Some(fp) => match fp.into_path() {
            Ok(path) => serde_json::json!({"ok": true, "path": path.to_string_lossy()}),
            Err(e) => serde_json::json!({"ok": false, "error": format!("path conversion: {e}")}),
        },
        None => serde_json::json!({"ok": false, "cancelled": true}),
    }
}

// ============================================================================
// Session 6 — auto-backup
//
// On app start, JS calls run_backup_check. If today's backup file doesn't
// exist (and conflict guard isn't blocking us), we copy masterstone.db to
// the backup folder with today's date in the filename. Old backups beyond
// the retention count are purged.
//
// Local default: ~/Library/Application Support/com.masterstone.crm/backups/
// OneDrive option: ~/Library/CloudStorage/OneDrive-Masterstone/Masterstone/backups/
// ============================================================================

fn default_backup_dir_local() -> Option<PathBuf> {
    let mut p = dirs::data_dir()?;
    p.push("com.masterstone.crm");
    p.push("backups");
    Some(p)
}

fn default_backup_dir_onedrive() -> Option<PathBuf> {
    snapshot_dir().map(|d| d.join("backups"))
}

fn resolve_backup_dir(folder_setting: &str, mode_setting: &str) -> Option<PathBuf> {
    if !folder_setting.is_empty() {
        return Some(PathBuf::from(folder_setting));
    }
    match mode_setting {
        "onedrive" => default_backup_dir_onedrive(),
        _ => default_backup_dir_local(),
    }
}

#[derive(Serialize)]
struct BackupResult {
    ok: bool,
    skipped: bool,
    skip_reason: Option<String>,
    error: Option<String>,
    path: Option<String>,
    bytes_written: Option<u64>,
    purged_count: usize,
}

#[tauri::command]
fn run_backup_check() -> BackupResult {
    // 1. Read the relevant settings (or fall back to defaults).
    let (folder_setting, mode_setting, retention) = read_backup_settings();

    let dir = match resolve_backup_dir(&folder_setting, &mode_setting) {
        Some(d) => d,
        None => return BackupResult {
            ok: false, skipped: false, skip_reason: None,
            error: Some("Could not resolve backup directory.".into()),
            path: None, bytes_written: None, purged_count: 0,
        },
    };

    if let Err(e) = std::fs::create_dir_all(&dir) {
        return BackupResult {
            ok: false, skipped: false, skip_reason: None,
            error: Some(format!("Could not create backup folder: {e}")),
            path: None, bytes_written: None, purged_count: 0,
        };
    }

    // 2. Today's filename.
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let filename = format!("masterstone_{date}.db");
    let target = dir.join(&filename);

    // If today's backup already exists, skip (one backup per day).
    if target.exists() {
        return BackupResult {
            ok: true, skipped: true,
            skip_reason: Some("Today's backup already exists.".into()),
            error: None,
            path: Some(target.to_string_lossy().to_string()),
            bytes_written: None, purged_count: 0,
        };
    }

    // 3. Source file (current .db).
    let source = match db_path() {
        Some(p) => p,
        None => return BackupResult {
            ok: false, skipped: false, skip_reason: None,
            error: Some("Could not resolve database path.".into()),
            path: None, bytes_written: None, purged_count: 0,
        },
    };
    if !source.exists() {
        return BackupResult {
            ok: false, skipped: false, skip_reason: None,
            error: Some("Database file does not exist (nothing to back up).".into()),
            path: None, bytes_written: None, purged_count: 0,
        };
    }

    // 4. Copy. Note: SQLite WAL files are sidecars. For a fully-safe backup
    // we'd checkpoint WAL into the main db first, but rusqlite's BACKUP API
    // is more reliable. For Session 6 simplicity, plain copy is acceptable —
    // the WAL is replayed automatically when the backup file is opened.
    let bytes = match std::fs::copy(&source, &target) {
        Ok(b) => b,
        Err(e) => return BackupResult {
            ok: false, skipped: false, skip_reason: None,
            error: Some(format!("Could not copy backup: {e}")),
            path: Some(target.to_string_lossy().to_string()),
            bytes_written: None, purged_count: 0,
        },
    };

    // 5. Purge older.
    let purged = purge_old_backups(&dir, retention);

    BackupResult {
        ok: true, skipped: false, skip_reason: None,
        error: None,
        path: Some(target.to_string_lossy().to_string()),
        bytes_written: Some(bytes),
        purged_count: purged,
    }
}

fn read_backup_settings() -> (String, String, usize) {
    let mut folder = String::new();
    let mut mode = "local".to_string();
    let mut retention = 14usize;
    if let Some(path) = db_path() {
        if let Ok(con) = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
            if let Ok(mut stmt) = con.prepare("SELECT key, value FROM app_settings WHERE key LIKE 'ms_app__%'") {
                if let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))) {
                    for row in rows.flatten() {
                        match row.0.as_str() {
                            "ms_app__backup_folder" => folder = row.1,
                            "ms_app__backup_destination_mode" => {
                                if row.1 == "onedrive" { mode = "onedrive".to_string(); }
                            },
                            "ms_app__backup_retention" => {
                                if let Ok(n) = row.1.parse::<usize>() {
                                    if n > 0 && n <= 365 { retention = n; }
                                }
                            },
                            _ => {}
                        }
                    }
                }
            }
        }
    }
    (folder, mode, retention)
}

fn purge_old_backups(dir: &PathBuf, keep: usize) -> usize {
    let mut backups: Vec<(PathBuf, u64)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if let Some(name) = p.file_name().and_then(|s| s.to_str()) {
                if name.starts_with("masterstone_") && name.ends_with(".db") {
                    if let Some(t) = file_mtime_secs(&p) {
                        backups.push((p, t));
                    }
                }
            }
        }
    }
    if backups.len() <= keep {
        return 0;
    }
    backups.sort_by(|a, b| b.1.cmp(&a.1)); // newest first
    let to_delete = &backups[keep..];
    let mut purged = 0;
    for (path, _) in to_delete {
        if std::fs::remove_file(path).is_ok() {
            purged += 1;
        }
    }
    purged
}

// ============================================================================
// Session 6 — conflict guard
//
// JS calls check_db_conflict when surfacing unsaved-write errors, or
// proactively before doing anything sensitive. If the .db's mtime has
// changed without our involvement, the user gets prompted to reload.
//
// After a reload-or-override decision, JS calls acknowledge_db_conflict
// which refreshes our recorded mtime and resets the connection so the next
// read sees the on-disk state.
// ============================================================================

#[derive(Serialize)]
struct ConflictResult {
    ok: bool,
    conflict: bool,
    detail: Option<String>,
    db_mtime: Option<u64>,
}

#[tauri::command]
fn check_db_conflict(state: tauri::State<DbState>) -> ConflictResult {
    // Open once if needed (so we have a recorded mtime baseline).
    let _ = state.ensure_open();
    let detected = state.detect_external_change();
    let db_mtime = db_path().and_then(|p| file_mtime_secs(&p));
    match detected {
        Some(true) => ConflictResult {
            ok: true, conflict: true,
            detail: Some("Database file was modified externally since this app session started.".into()),
            db_mtime,
        },
        Some(false) => ConflictResult { ok: true, conflict: false, detail: None, db_mtime },
        None => ConflictResult { ok: true, conflict: false, detail: Some("No baseline mtime recorded yet.".into()), db_mtime },
    }
}

#[tauri::command]
fn acknowledge_db_conflict(state: tauri::State<DbState>, action: String) -> serde_json::Value {
    // action = "reload"  → close connection so next ensure_open re-reads file
    //        = "override" → just refresh mtime, keep connection
    match action.as_str() {
        "reload" => {
            state.reset();
            serde_json::json!({"ok": true, "action": "reload"})
        },
        "override" => {
            state.refresh_mtime();
            serde_json::json!({"ok": true, "action": "override"})
        },
        _ => serde_json::json!({"ok": false, "error": format!("Unknown action: {action}")}),
    }
}

// ============================================================================
// Session 6 — attachment file extraction
//
// One-time migration: company_profile.raw_data has logoDataUrl,
// letterheadDataUrl, signatureImageDataUrl as inline base64 (~720 KB total).
// Move each to a file under attachments/ keyed by content hash. Replace the
// inline base64 in the JSON with a sentinel "ms-attachment://<hash>.<ext>".
//
// On app boot, the load path reads each sentinel back and re-injects the
// base64 from disk into localStorage so the CRM continues to read
// logoDataUrl etc. as data URLs.
// ============================================================================

fn attachments_dir() -> Option<PathBuf> {
    let mut p = dirs::data_dir()?;
    p.push("com.masterstone.crm");
    p.push("attachments");
    Some(p)
}

/// Detect mime → file extension mapping from a data URL prefix.
fn mime_to_ext(mime: &str) -> &'static str {
    match mime {
        "image/png"  => "png",
        "image/jpeg" => "jpg",
        "image/jpg"  => "jpg",
        "image/gif"  => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        _ => "bin",
    }
}

/// Cheap content fingerprint — first 12 chars of base64 + length.
/// Not cryptographic; just unique-enough for deduplication.
fn fingerprint(s: &str) -> String {
    let head: String = s.chars().take(12).filter(|c| c.is_ascii_alphanumeric()).collect();
    format!("{}_{}", head, s.len())
}

/// Returns Some((sentinel, written_bytes)) if extraction was performed,
/// None if the value was already a sentinel or empty.
fn extract_one_data_url(data_url: &str, dir: &PathBuf) -> Option<(String, u64)> {
    if data_url.is_empty() || data_url.starts_with("ms-attachment://") {
        return None;
    }
    if !data_url.starts_with("data:") {
        return None;
    }
    let comma = data_url.find(',')?;
    let header = &data_url[5..comma]; // strip "data:"
    let body = &data_url[comma + 1..];
    // header is like "image/png;base64"
    let mime = header.split(';').next().unwrap_or("application/octet-stream");
    let ext = mime_to_ext(mime);
    let name = format!("{}.{}", fingerprint(body), ext);
    let target = dir.join(&name);
    if !target.exists() {
        // Decode base64. We need a base64 decoder. Use a tiny inline one
        // to avoid adding a new crate dependency for one use.
        let decoded = base64_decode(body)?;
        if std::fs::write(&target, &decoded).is_err() {
            return None;
        }
    }
    let bytes = std::fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
    Some((format!("ms-attachment://{}", name), bytes))
}

/// Minimal base64 decoder (RFC 4648 standard alphabet, no whitespace tolerance
/// beyond a few ASCII whitespace chars). Avoids adding a base64 crate dep.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn ch_val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes: Vec<u8> = s.bytes().filter(|b| !matches!(b, b'\n' | b'\r' | b' ' | b'\t')).collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4 + 4);
    let mut i = 0;
    while i + 4 <= bytes.len() {
        let a = ch_val(bytes[i])?;
        let b = ch_val(bytes[i + 1])?;
        let c_byte = bytes[i + 2];
        let d_byte = bytes[i + 3];
        out.push((a << 2) | (b >> 4));
        if c_byte != b'=' {
            let c = ch_val(c_byte)?;
            out.push(((b & 0x0F) << 4) | (c >> 2));
            if d_byte != b'=' {
                let d = ch_val(d_byte)?;
                out.push(((c & 0x03) << 6) | d);
            }
        }
        i += 4;
    }
    Some(out)
}

#[derive(Serialize)]
struct ExtractResult {
    ok: bool,
    error: Option<String>,
    extracted_count: usize,
    bytes_saved: u64,
    skipped_already_extracted: bool,
}

#[tauri::command]
fn extract_attachments(state: tauri::State<DbState>) -> ExtractResult {
    let dir = match attachments_dir() {
        Some(d) => d,
        None => return ExtractResult {
            ok: false,
            error: Some("Could not resolve attachments dir".into()),
            extracted_count: 0, bytes_saved: 0, skipped_already_extracted: false,
        },
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return ExtractResult {
            ok: false,
            error: Some(format!("Could not create attachments dir: {e}")),
            extracted_count: 0, bytes_saved: 0, skipped_already_extracted: false,
        };
    }

    // Read company_profile.raw_data
    let raw = match with_writer(&state, |con| {
        con.query_row("SELECT raw_data FROM company_profile WHERE id = 1", [], |r| r.get::<_, String>(0))
    }) {
        Ok(s) => s,
        Err(e) => return ExtractResult {
            ok: false,
            error: Some(format!("Could not read company_profile: {e}")),
            extracted_count: 0, bytes_saved: 0, skipped_already_extracted: false,
        },
    };

    let mut profile: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => return ExtractResult {
            ok: false,
            error: Some(format!("Invalid company_profile JSON: {e}")),
            extracted_count: 0, bytes_saved: 0, skipped_already_extracted: false,
        },
    };

    let target_keys = ["logoDataUrl", "letterheadDataUrl", "signatureImageDataUrl"];
    let mut count = 0usize;
    let mut bytes = 0u64;
    let mut all_already_extracted = true;
    let obj = match profile.as_object_mut() {
        Some(o) => o,
        None => return ExtractResult {
            ok: false,
            error: Some("company_profile is not an object".into()),
            extracted_count: 0, bytes_saved: 0, skipped_already_extracted: false,
        },
    };

    for key in &target_keys {
        let Some(val) = obj.get(*key).and_then(|v| v.as_str()) else { continue; };
        if val.is_empty() {
            continue;
        }
        if val.starts_with("ms-attachment://") {
            continue; // already extracted
        }
        all_already_extracted = false;
        if let Some((sentinel, n)) = extract_one_data_url(val, &dir) {
            obj.insert((*key).to_string(), serde_json::Value::String(sentinel));
            count += 1;
            bytes += n;
        }
    }

    if count == 0 && all_already_extracted {
        return ExtractResult {
            ok: true, error: None,
            extracted_count: 0, bytes_saved: 0,
            skipped_already_extracted: true,
        };
    }

    if count == 0 {
        // Nothing extracted but not because already-extracted —
        // could be empty fields. No-op success.
        return ExtractResult {
            ok: true, error: None,
            extracted_count: 0, bytes_saved: 0,
            skipped_already_extracted: false,
        };
    }

    // Write the updated profile back.
    let new_raw = profile.to_string();
    if let Err(e) = with_writer(&state, |con| {
        con.execute(
            "UPDATE company_profile SET raw_data = ?1, modified_at = ?2 WHERE id = 1",
            params![new_raw, now_iso()],
        )?;
        Ok(())
    }) {
        return ExtractResult {
            ok: false,
            error: Some(format!("Could not update company_profile: {e}")),
            extracted_count: count, bytes_saved: bytes, skipped_already_extracted: false,
        };
    }

    ExtractResult {
        ok: true, error: None,
        extracted_count: count, bytes_saved: bytes, skipped_already_extracted: false,
    }
}

/// On the load path, JS calls this after storage_load_all to re-inject
/// extracted attachments back into localStorage as data URLs. Returns a
/// dictionary of { "logoDataUrl": "data:image/...", ... } that JS merges
/// into the company_profile JSON in localStorage.
#[tauri::command]
fn load_attachments_into_data_urls() -> serde_json::Value {
    let dir = match attachments_dir() {
        Some(d) => d,
        None => return serde_json::json!({"ok": false, "error": "no attachments dir"}),
    };
    if !dir.exists() {
        return serde_json::json!({"ok": true, "attachments": {}});
    }

    let path = match db_path() {
        Some(p) => p,
        None => return serde_json::json!({"ok": false, "error": "no db path"}),
    };
    let con = match Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(c) => c,
        Err(e) => return serde_json::json!({"ok": false, "error": format!("open: {e}")}),
    };
    let raw: String = match con.query_row("SELECT raw_data FROM company_profile WHERE id = 1", [], |r| r.get(0)) {
        Ok(s) => s,
        Err(_) => return serde_json::json!({"ok": true, "attachments": {}}),
    };
    let profile: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return serde_json::json!({"ok": true, "attachments": {}}),
    };

    let target_keys = ["logoDataUrl", "letterheadDataUrl", "signatureImageDataUrl"];
    let mut out = serde_json::Map::new();
    for key in &target_keys {
        let Some(val) = profile.get(*key).and_then(|v| v.as_str()) else { continue; };
        if !val.starts_with("ms-attachment://") {
            continue;
        }
        let name = &val["ms-attachment://".len()..];
        // Defense: only accept simple filenames, no path separators
        if name.contains('/') || name.contains('\\') || name.contains("..") {
            continue;
        }
        let file_path = dir.join(name);
        let Ok(bytes) = std::fs::read(&file_path) else { continue; };
        let ext = name.rsplit('.').next().unwrap_or("bin");
        let mime = match ext.to_lowercase().as_str() {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "svg" => "image/svg+xml",
            _ => "application/octet-stream",
        };
        let b64 = base64_encode(&bytes);
        let data_url = format!("data:{};base64,{}", mime, b64);
        out.insert((*key).to_string(), serde_json::Value::String(data_url));
    }
    serde_json::json!({"ok": true, "attachments": out})
}

/// Minimal base64 encoder (matches base64_decode above).
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((bytes.len() * 4 / 3) + 4);
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let a = bytes[i];
        let b = bytes[i + 1];
        let c = bytes[i + 2];
        out.push(ALPHA[(a >> 2) as usize] as char);
        out.push(ALPHA[(((a & 0x03) << 4) | (b >> 4)) as usize] as char);
        out.push(ALPHA[(((b & 0x0F) << 2) | (c >> 6)) as usize] as char);
        out.push(ALPHA[(c & 0x3F) as usize] as char);
        i += 3;
    }
    let rem = bytes.len() - i;
    if rem == 1 {
        let a = bytes[i];
        out.push(ALPHA[(a >> 2) as usize] as char);
        out.push(ALPHA[((a & 0x03) << 4) as usize] as char);
        out.push('=');
        out.push('=');
    } else if rem == 2 {
        let a = bytes[i];
        let b = bytes[i + 1];
        out.push(ALPHA[(a >> 2) as usize] as char);
        out.push(ALPHA[(((a & 0x03) << 4) | (b >> 4)) as usize] as char);
        out.push(ALPHA[((b & 0x0F) << 2) as usize] as char);
        out.push('=');
    }
    out
}


// ============================================================================
// Session 8 — Files folder structure + document categories
//
// Replaces the broken Session 7 download approach with a local-files-first
// model. The user dumps files into a "Migration/" folder, the matching tool
// (Session 8 Build B, separate deliverable) proposes record matches, and on
// confirmation files are moved to their permanent home in:
//
//   <files_root>/<FY-folder>/<category-path>/
//
// Defaults:
//   files_root        = OneDrive-Masterstone/Masterstone/Files/
//   migration_root    = OneDrive-Masterstone/Masterstone/Migration/
//   FY folders        = FY20-21 through FY26-27 (7 folders)
//   default categories: Proposals, POs/Internal, POs/Client, Invoices/Client,
//                       Invoices/OEM (5 categories)
//
// Categories are stored in app_settings under key "ms_app__document_categories"
// as a JSON array. Users can add/remove categories via Settings UI.
// Adding a category creates the folder under every existing FY.
// ============================================================================

/// The 7 financial year folder names used by Build 2A.
/// Format: FYxx-yy where xx is start year (2 digits) and yy is end year (2 digits).
const FY_FOLDERS: &[&str] = &[
    "FY20-21", "FY21-22", "FY22-23", "FY23-24",
    "FY24-25", "FY25-26", "FY26-27",
];

/// Default document categories seeded on first initialization.
/// Each tuple = (key, display_name, path_segments).
///
/// As of Build B-fix-2 the destination path uses category-first hierarchy:
///   Files/{category}/{FY}/file.pdf
/// (was Files/{FY}/{category}/file.pdf in earlier builds — auto-migrated)
fn default_categories() -> Vec<(&'static str, &'static str, Vec<&'static str>)> {
    vec![
        ("proposals",         "Proposals",         vec!["Proposals"]),
        ("pos_internal",      "POs / Internal",    vec!["POs", "Internal"]),
        ("pos_client",        "POs / Client",      vec!["POs", "Client"]),
        ("sales_invoices",    "Sales Invoices",    vec!["Sales Invoices"]),
        ("purchase_invoices", "Purchase Invoices", vec!["Purchase Invoices"]),
    ]
}

/// Resolve the configured files root. Defaults to
/// OneDrive-Masterstone/Masterstone/Files/.
fn files_root() -> Option<PathBuf> {
    snapshot_dir().map(|p| p.join("Files"))
}

/// Resolve the migration dump folder. User drops unsorted files here.
fn migration_root() -> Option<PathBuf> {
    snapshot_dir().map(|p| p.join("Migration"))
}

/// Document category record stored in app_settings JSON.
#[derive(Clone, Serialize, serde::Deserialize)]
struct DocumentCategory {
    key: String,
    display_name: String,
    path_segments: Vec<String>,
    /// True if this is a default category (not user-added). Defaults can be
    /// renamed but not removed.
    is_default: bool,
}

/// Load the configured document categories from app_settings.
/// On first run (no setting present), returns the seeded default list.
///
/// As of Build B-fix-2: if stored categories use the old keys
/// (invoices_client, invoices_oem), they are auto-migrated to the new
/// keys/names/paths in memory. The next save persists the migration.
fn load_document_categories() -> Vec<DocumentCategory> {
    let path = match db_path() {
        Some(p) => p,
        None => return seed_default_categories(),
    };
    if !path.exists() {
        return seed_default_categories();
    }
    let con = match Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(c) => c,
        Err(_) => return seed_default_categories(),
    };
    let raw: String = match con.query_row(
        "SELECT value FROM app_settings WHERE key = 'ms_app__document_categories'",
        [],
        |r| r.get(0),
    ) {
        Ok(v) => v,
        Err(_) => return seed_default_categories(),
    };
    let mut list: Vec<DocumentCategory> = match serde_json::from_str::<Vec<DocumentCategory>>(&raw) {
        Ok(list) if !list.is_empty() => list,
        _ => return seed_default_categories(),
    };
    // Build B-fix-2 in-memory migration: rename old keys to new ones.
    for cat in list.iter_mut() {
        match cat.key.as_str() {
            "invoices_client" => {
                cat.key = "sales_invoices".to_string();
                cat.display_name = "Sales Invoices".to_string();
                cat.path_segments = vec!["Sales Invoices".to_string()];
            }
            "invoices_oem" => {
                cat.key = "purchase_invoices".to_string();
                cat.display_name = "Purchase Invoices".to_string();
                cat.path_segments = vec!["Purchase Invoices".to_string()];
            }
            _ => {}
        }
    }
    list
}

fn seed_default_categories() -> Vec<DocumentCategory> {
    default_categories().into_iter().map(|(k, n, p)| DocumentCategory {
        key: k.to_string(),
        display_name: n.to_string(),
        path_segments: p.iter().map(|s| s.to_string()).collect(),
        is_default: true,
    }).collect()
}

/// Persist a category list to app_settings. Caller is responsible for
/// validation; this just writes whatever it's given.
fn save_document_categories(cats: &[DocumentCategory]) -> Result<(), String> {
    let json = serde_json::to_string(cats).map_err(|e| format!("serialize: {e}"))?;
    let dbp = db_path().ok_or("no db path")?;
    let con = Connection::open_with_flags(&dbp, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .map_err(|e| format!("open: {e}"))?;
    let _: String = con.query_row("PRAGMA journal_mode = WAL;", [], |r| r.get(0))
        .map_err(|e| format!("wal: {e}"))?;
    con.execute(
        "INSERT INTO app_settings (key, value, modified_at)
         VALUES ('ms_app__document_categories', ?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, modified_at = excluded.modified_at",
        params![json, now_iso()],
    ).map_err(|e| format!("write: {e}"))?;
    Ok(())
}

/// Validate a path segment for safety.
/// Disallows: empty, ., .., absolute paths, slashes, special chars.
/// Returns Ok(()) if safe, Err with reason otherwise.
fn validate_path_segment(seg: &str) -> Result<(), String> {
    let s = seg.trim();
    if s.is_empty() {
        return Err("Path segment cannot be empty.".into());
    }
    if s == "." || s == ".." {
        return Err("Path segment cannot be . or ..".into());
    }
    if s.contains('/') || s.contains('\\') {
        return Err("Path segment cannot contain slashes.".into());
    }
    if s.contains('\0') {
        return Err("Path segment contains null character.".into());
    }
    if s.starts_with('.') {
        return Err("Path segment cannot start with a dot.".into());
    }
    // Reject characters that don't behave on macOS/Windows in filenames.
    let bad_chars = ['<', '>', ':', '"', '|', '?', '*'];
    for c in bad_chars {
        if s.contains(c) {
            return Err(format!("Path segment cannot contain '{c}'."));
        }
    }
    if s.len() > 80 {
        return Err("Path segment too long (max 80 chars).".into());
    }
    Ok(())
}

/// Validate a category key — used as a stable identifier in settings.
/// Lowercase letters, digits, underscores only.
fn validate_category_key(key: &str) -> Result<(), String> {
    if key.is_empty() {
        return Err("Category key cannot be empty.".into());
    }
    if key.len() > 40 {
        return Err("Category key too long (max 40 chars).".into());
    }
    for c in key.chars() {
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
            return Err("Category key must use only lowercase letters, digits, and underscores.".into());
        }
    }
    Ok(())
}

#[derive(Serialize)]
struct InitFoldersResult {
    ok: bool,
    error: Option<String>,
    /// Number of folders created (newly). Already-existing folders are not counted.
    created_count: usize,
    /// Number of folders that already existed (no-op).
    skipped_count: usize,
    /// Absolute path to the files root.
    files_root: Option<String>,
    /// Absolute path to the migration root.
    migration_root: Option<String>,
}

#[tauri::command]
fn initialize_files_folders() -> InitFoldersResult {
    // 1. Resolve roots.
    let files = match files_root() {
        Some(p) => p,
        None => return InitFoldersResult {
            ok: false, error: Some("Could not resolve OneDrive-Masterstone folder.".into()),
            created_count: 0, skipped_count: 0,
            files_root: None, migration_root: None,
        },
    };
    let migration = match migration_root() {
        Some(p) => p,
        None => return InitFoldersResult {
            ok: false, error: Some("Could not resolve OneDrive-Masterstone folder.".into()),
            created_count: 0, skipped_count: 0,
            files_root: Some(files.to_string_lossy().to_string()), migration_root: None,
        },
    };

    let categories = load_document_categories();
    let mut created = 0usize;
    let mut skipped = 0usize;

    // 2. Create files_root and migration_root.
    for r in [&files, &migration] {
        let existed = r.exists();
        if let Err(e) = std::fs::create_dir_all(r) {
            return InitFoldersResult {
                ok: false, error: Some(format!("Could not create {}: {}", r.display(), e)),
                created_count: created, skipped_count: skipped,
                files_root: Some(files.to_string_lossy().to_string()),
                migration_root: Some(migration.to_string_lossy().to_string()),
            };
        }
        if existed { skipped += 1; } else { created += 1; }
    }

    // 3. Create category × FY folders (category first, FY second).
    //    Layout: Files/{category-path}/{FY}/
    //    e.g. Files/Sales Invoices/FY24-25/, Files/POs/Internal/FY24-25/
    for cat in &categories {
        for fy in FY_FOLDERS {
            let mut p = files.clone();
            for seg in &cat.path_segments {
                p.push(seg);
            }
            p.push(fy);
            let existed = p.exists();
            if let Err(e) = std::fs::create_dir_all(&p) {
                return InitFoldersResult {
                    ok: false, error: Some(format!("Could not create {}: {}", p.display(), e)),
                    created_count: created, skipped_count: skipped,
                    files_root: Some(files.to_string_lossy().to_string()),
                    migration_root: Some(migration.to_string_lossy().to_string()),
                };
            }
            if existed { skipped += 1; } else { created += 1; }
        }
    }

    InitFoldersResult {
        ok: true, error: None,
        created_count: created, skipped_count: skipped,
        files_root: Some(files.to_string_lossy().to_string()),
        migration_root: Some(migration.to_string_lossy().to_string()),
    }
}

#[derive(Serialize)]
struct FilesSetupStatus {
    ok: bool,
    initialized: bool,
    files_root_exists: bool,
    migration_root_exists: bool,
    files_root: Option<String>,
    migration_root: Option<String>,
    fy_folders_present: usize,
    fy_folders_total: usize,
    category_folders_present: usize,
    category_folders_total: usize,
    /// Number of files currently sitting in the Migration folder, awaiting matching.
    /// Counted recursively. Folders are not counted.
    migration_pending_count: usize,
}

#[tauri::command]
fn get_files_setup_status() -> FilesSetupStatus {
    let files = files_root();
    let migration = migration_root();
    let files_str = files.as_ref().map(|p| p.to_string_lossy().to_string());
    let migration_str = migration.as_ref().map(|p| p.to_string_lossy().to_string());
    let files_root_exists = files.as_ref().map(|p| p.exists()).unwrap_or(false);
    let migration_root_exists = migration.as_ref().map(|p| p.exists()).unwrap_or(false);

    let categories = load_document_categories();
    let mut fy_present = 0usize;
    let mut cat_present = 0usize;
    let fy_total = FY_FOLDERS.len();
    let cat_total = FY_FOLDERS.len() * categories.len();

    if let Some(ref f) = files {
        for fy in FY_FOLDERS {
            let fy_path = f.join(fy);
            if fy_path.exists() {
                fy_present += 1;
                for cat in &categories {
                    let mut p = fy_path.clone();
                    for seg in &cat.path_segments {
                        p.push(seg);
                    }
                    if p.exists() { cat_present += 1; }
                }
            }
        }
    }

    let migration_pending = migration.as_ref()
        .map(|p| count_files_recursive(p))
        .unwrap_or(0);

    FilesSetupStatus {
        ok: true,
        initialized: files_root_exists && migration_root_exists && fy_present == fy_total,
        files_root_exists,
        migration_root_exists,
        files_root: files_str,
        migration_root: migration_str,
        fy_folders_present: fy_present,
        fy_folders_total: fy_total,
        category_folders_present: cat_present,
        category_folders_total: cat_total,
        migration_pending_count: migration_pending,
    }
}

/// Recursively count files (not directories) under a path. Used for the
/// "files awaiting matching" count in the Settings UI.
fn count_files_recursive(p: &PathBuf) -> usize {
    if !p.exists() { return 0; }
    let mut count = 0usize;
    let mut stack: Vec<PathBuf> = vec![p.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue; };
        for entry in entries.flatten() {
            let path = entry.path();
            // Skip macOS metadata files
            if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                if name == ".DS_Store" || name.starts_with("._") {
                    continue;
                }
            }
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => stack.push(path),
                Ok(ft) if ft.is_file() => count += 1,
                _ => {}
            }
        }
    }
    count
}

#[tauri::command]
fn get_document_categories() -> serde_json::Value {
    let cats = load_document_categories();
    serde_json::json!({"ok": true, "categories": cats})
}

#[derive(Serialize)]
struct AddCategoryResult {
    ok: bool,
    error: Option<String>,
    folders_created: usize,
}

#[tauri::command]
fn add_document_category(
    key: String,
    display_name: String,
    path_segments: Vec<String>,
) -> AddCategoryResult {
    // 1. Validate inputs.
    if let Err(e) = validate_category_key(&key) {
        return AddCategoryResult { ok: false, error: Some(e), folders_created: 0 };
    }
    if display_name.trim().is_empty() || display_name.len() > 80 {
        return AddCategoryResult {
            ok: false, error: Some("Display name must be 1-80 characters.".into()),
            folders_created: 0,
        };
    }
    if path_segments.is_empty() {
        return AddCategoryResult {
            ok: false, error: Some("At least one path segment required.".into()),
            folders_created: 0,
        };
    }
    if path_segments.len() > 4 {
        return AddCategoryResult {
            ok: false, error: Some("Path nesting too deep (max 4 levels).".into()),
            folders_created: 0,
        };
    }
    for seg in &path_segments {
        if let Err(e) = validate_path_segment(seg) {
            return AddCategoryResult { ok: false, error: Some(e), folders_created: 0 };
        }
    }

    // 2. Load existing categories, refuse if key already used.
    let mut cats = load_document_categories();
    if cats.iter().any(|c| c.key == key) {
        return AddCategoryResult {
            ok: false, error: Some(format!("A category with key '{key}' already exists.")),
            folders_created: 0,
        };
    }

    // 3. Add the new category, save.
    cats.push(DocumentCategory {
        key: key.clone(),
        display_name: display_name.trim().to_string(),
        path_segments: path_segments.iter().map(|s| s.trim().to_string()).collect(),
        is_default: false,
    });
    if let Err(e) = save_document_categories(&cats) {
        return AddCategoryResult { ok: false, error: Some(e), folders_created: 0 };
    }

    // 4. Create folders under every existing FY.
    let files = match files_root() {
        Some(p) => p,
        None => return AddCategoryResult {
            ok: true, error: None, folders_created: 0, // settings saved, but folders skipped
        },
    };
    let mut created = 0usize;
    for fy in FY_FOLDERS {
        let mut p = files.clone();
        p.push(fy);
        // Only create category folder if FY folder exists. Otherwise skip;
        // the user hasn't initialized yet, and a future initialize call
        // will pick up the new category from settings.
        if !p.exists() { continue; }
        for seg in &path_segments {
            p.push(seg.trim());
        }
        if !p.exists() {
            if std::fs::create_dir_all(&p).is_ok() {
                created += 1;
            }
        }
    }

    AddCategoryResult { ok: true, error: None, folders_created: created }
}

#[tauri::command]
fn remove_document_category(key: String) -> serde_json::Value {
    let mut cats = load_document_categories();
    let original_len = cats.len();
    let target = match cats.iter().find(|c| c.key == key) {
        Some(c) => c.clone(),
        None => return serde_json::json!({"ok": false, "error": format!("No category with key '{key}'.")}),
    };
    if target.is_default {
        return serde_json::json!({
            "ok": false,
            "error": "Default categories cannot be removed (only user-added ones)."
        });
    }
    cats.retain(|c| c.key != key);
    if cats.len() == original_len {
        return serde_json::json!({"ok": false, "error": "Category not found."});
    }
    if let Err(e) = save_document_categories(&cats) {
        return serde_json::json!({"ok": false, "error": e});
    }
    // Note: we deliberately do NOT delete the folders on disk. They may
    // contain user files. The user can delete them manually if they wish.
    serde_json::json!({
        "ok": true,
        "removed_category": target.key,
        "folders_left_intact": "Existing folders on disk were preserved. Delete manually if no longer needed."
    })
}

/// Open a folder or file in Finder / default app. Used from the Settings
/// UI to let the user click through to the Migration or Files folder.
#[tauri::command]
fn reveal_path<R: tauri::Runtime>(app: tauri::AppHandle<R>, path: String) -> serde_json::Value {
    if path.is_empty() {
        return serde_json::json!({"ok": false, "error": "Empty path"});
    }
    let pb = PathBuf::from(&path);
    if !pb.exists() {
        return serde_json::json!({"ok": false, "error": "Path does not exist."});
    }
    match app.opener().open_path(path.clone(), None::<&str>) {
        Ok(_) => serde_json::json!({"ok": true, "path": path}),
        Err(e) => serde_json::json!({"ok": false, "error": format!("{e}")}),
    }
}




// ============================================================================
// Session 8 Build B (rev) — Migration matcher (improved)
//
// Build B-fix improvements (vs the original Build B):
//
//   1. Filename product-code aliases — user can teach the matcher that
//      "AGE" means "DT Email", "EIS" means "DT Network", etc. Stored in
//      app_settings and exposed via Settings UI.
//
//   2. Date extraction (DD_MM_YYYY) — pull a date from the filename and
//      score it against the contract's relevant date field (clientPODate,
//      internalPODate, billDate, actualInvoiceDate, oemInvoiceDate).
//      Same date = strong; ±2 days = good; same month = weak.
//
//   3. Document number aware matching — both the doc number's normalized
//      form AND its original-with-separators form are tried as substrings.
//      So "22-23/06/DT130" matches "22_23_06_DT130", "222306DT130", etc.
//
//   4. Client short name from client master — fixes the bug where the
//      original Build B forgot to read shortName from ms_client_master_v1.
//      Plus splits client name into tokens for fragmentary matching
//      ("63 Moons Technologies Limited" → tries "63 Moons", "63Moons",
//      "Moons", "Technologies").
//
//   5. OEM aliases — short codes like DT, VRS, DTSAAS for OEM names. Also
//      stored in app_settings.
//
//   6. Lowered auto-confirm threshold to 0.85 (was 0.9) so high-but-
//      not-perfect matches don't get stuck in review.
//
//   7. Lowered unmatched threshold to 0.20 (was 0.30). Weak-matches still
//      surface in review with alternatives, instead of vanishing.
//
//   8. URL deduplication — if two contracts share the same OneDrive URL
//      across fields, the matcher picks the most-specific candidate
//      (billingYear-scoped invoice over generic PO link).
//
//   9. list_linkable_record_fields now ALWAYS returns every field; JS
//      decides whether to filter by `already_has_local`. The "Override"
//      toggle in the link picker shows/hides fields with existing locals.
//
//  10. Diagnostic command get_local_field_status — counts how many
//      fields per contract have _local set. Useful for debugging and
//      progress dashboards.
// ============================================================================

/// One file discovered in the Migration folder.
#[derive(Clone, Serialize)]
struct MigrationFile {
    abs_path: String,
    filename: String,
    filename_lc: String,
    size_bytes: u64,
}

/// One candidate match for a file → CRM link field pairing.
#[derive(Clone, Serialize)]
struct MatchCandidate {
    field_path: String,
    label: String,
    confidence: f64,
    reasons: Vec<String>,
    original_url: String,
    category_path: Vec<String>,
    suggested_fy: Option<String>,
    document_type: String,
}

#[derive(Serialize)]
struct FileWithMatches {
    file: MigrationFile,
    candidates: Vec<MatchCandidate>,
    bucket: String, // "auto" | "review" | "unmatched"
}

#[derive(Clone)]
enum FieldKind {
    InternalPO,
    ClientPOLegacy,
    ClientPOArr(usize),
    ClientInvoice(usize),
    OemInvoice(usize),
}

/// Convert YYYY-MM-DD to "FYxx-yy" (Indian financial year).
fn date_to_fy(date_str: &str) -> Option<String> {
    if date_str.len() < 7 { return None; }
    let year: i32 = date_str.get(0..4)?.parse().ok()?;
    let month: i32 = date_str.get(5..7)?.parse().ok()?;
    if !(1..=12).contains(&month) { return None; }
    if !(2000..=2099).contains(&year) { return None; }
    let (start, end) = if month >= 4 { (year, year + 1) } else { (year - 1, year) };
    Some(format!("FY{:02}-{:02}", start % 100, end % 100))
}

/// Convert YYYY-MM-DD to a "days since 2000-01-01" integer for cheap
/// date-distance comparisons. Imprecise for leap years etc., but fine
/// for our ±2-day windowing.
fn date_to_day_number(date_str: &str) -> Option<i64> {
    if date_str.len() < 10 { return None; }
    let year: i64 = date_str.get(0..4)?.parse().ok()?;
    let month: i64 = date_str.get(5..7)?.parse().ok()?;
    let day: i64 = date_str.get(8..10)?.parse().ok()?;
    // Approximate Julian: (year - 2000) * 365 + (month - 1) * 30 + day.
    // Imprecise but consistent.
    Some((year - 2000) * 365 + (month - 1) * 30 + day)
}

/// Extract a date from the filename in DD_MM_YYYY, DD-MM-YYYY, or
/// DD MM YYYY format. Returns canonical YYYY-MM-DD or None.
fn extract_filename_date(filename: &str) -> Option<String> {
    // Look for any 3-number sequence with separators.
    let bytes = filename.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        // Read first number.
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() { i += 1; }
        let n1_str = &filename[start..i];
        // Read separator.
        if i >= bytes.len() || !is_date_sep(bytes[i]) {
            i += 1;
            continue;
        }
        let sep = bytes[i];
        i += 1;
        let s2 = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() { i += 1; }
        let n2_str = &filename[s2..i];
        if i >= bytes.len() || bytes[i] != sep {
            // not a triple — keep scanning
            continue;
        }
        i += 1;
        let s3 = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() { i += 1; }
        let n3_str = &filename[s3..i];

        // Try to interpret as a date.
        let (n1, n2, n3) = match (n1_str.parse::<i32>(), n2_str.parse::<i32>(), n3_str.parse::<i32>()) {
            (Ok(a), Ok(b), Ok(c)) => (a, b, c),
            _ => continue,
        };

        // Plausibility: third number is the year (4-digit) and first
        // is the day (1-31), second is the month (1-12).
        if n3_str.len() == 4 && n3 >= 2000 && n3 <= 2099
            && n1 >= 1 && n1 <= 31 && n2 >= 1 && n2 <= 12 {
            return Some(format!("{:04}-{:02}-{:02}", n3, n2, n1));
        }
        // Or YYYY-MM-DD form (first is year)
        if n1_str.len() == 4 && n1 >= 2000 && n1 <= 2099
            && n2 >= 1 && n2 <= 12 && n3 >= 1 && n3 <= 31 {
            return Some(format!("{:04}-{:02}-{:02}", n1, n2, n3));
        }
    }
    None
}

fn is_date_sep(c: u8) -> bool { c == b'_' || c == b'-' || c == b' ' || c == b'.' || c == b'/' }

/// Lowercase + strip non-alphanumeric. "63 Moons Tech." -> "63moonstech".
fn normalize_for_match(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(|c| c.to_lowercase()).collect()
}

/// Returns true if `needle` (already normalized) appears as a substring of
/// `haystack` (already normalized). Empty needle matches nothing.
fn norm_contains(haystack: &str, needle: &str) -> bool {
    !needle.is_empty() && haystack.contains(needle)
}

/// Token-split a client/OEM name on whitespace and punctuation. Keeps
/// tokens of length >= 3.
fn name_tokens(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 3)
        .map(|t| t.to_lowercase())
        .collect()
}

/// Read product-code aliases from app_settings. Format: JSON array of
/// {"code": "AGE", "products": ["DT Email"]}.
fn load_product_code_aliases() -> Vec<(String, Vec<String>)> {
    let dbp = match db_path() { Some(p) => p, None => return vec![] };
    let con = match Connection::open_with_flags(&dbp, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(c) => c, Err(_) => return vec![],
    };
    let raw: String = match con.query_row(
        "SELECT value FROM app_settings WHERE key = 'ms_app__product_code_aliases'",
        [], |r| r.get(0),
    ) { Ok(s) => s, Err(_) => return vec![] };
    let v: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v, Err(_) => return vec![],
    };
    let arr = match v.as_array() { Some(a) => a, None => return vec![] };
    let mut out = Vec::new();
    for item in arr {
        let code = item.get("code").and_then(|x| x.as_str()).unwrap_or("");
        let products = item.get("products").and_then(|x| x.as_array());
        if code.is_empty() { continue; }
        let prods: Vec<String> = match products {
            Some(p) => p.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
            None => continue,
        };
        if !prods.is_empty() { out.push((code.to_lowercase(), prods)); }
    }
    out
}

/// Read OEM-name aliases from app_settings. Format: JSON object mapping
/// short code -> canonical OEM display name. e.g. {"DT": "Darktrace"}.
fn load_oem_aliases() -> Vec<(String, String)> {
    let dbp = match db_path() { Some(p) => p, None => return default_oem_aliases() };
    let con = match Connection::open_with_flags(&dbp, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(c) => c, Err(_) => return default_oem_aliases(),
    };
    let raw: String = match con.query_row(
        "SELECT value FROM app_settings WHERE key = 'ms_app__oem_aliases'",
        [], |r| r.get(0),
    ) { Ok(s) => s, Err(_) => return default_oem_aliases() };
    let v: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v, Err(_) => return default_oem_aliases(),
    };
    let obj = match v.as_object() { Some(o) => o, None => return default_oem_aliases() };
    let mut out = Vec::new();
    for (k, val) in obj {
        if let Some(s) = val.as_str() {
            out.push((k.to_lowercase(), s.to_string()));
        }
    }
    if out.is_empty() { default_oem_aliases() } else { out }
}

fn default_oem_aliases() -> Vec<(String, String)> {
    // Sensible defaults for the user's known OEMs.
    vec![
        ("dt".to_string(), "Darktrace".to_string()),
        ("vrs".to_string(), "Varonis".to_string()),
        ("varonis".to_string(), "Varonis".to_string()),
        ("darktrace".to_string(), "Darktrace".to_string()),
    ]
}

// ============================================================================
// Build B-fix-4: PDF text extraction
//
// Uses the pdf-extract crate to pull body text from PDF files. Returns None
// for scanned (image-only) PDFs where text extraction yields effectively
// nothing. The extracted text is then used by the matcher to find:
//   - exact doc numbers (PO numbers, invoice numbers)
//   - client GSTIN / PAN
//   - dates in the body
//   - OEM references
// These are dramatically more reliable than filename-only matching.
// ============================================================================

/// Try to extract text from a PDF file. Returns lowercased text on success,
/// or None if the file is unreadable or appears to be image-only (very
/// short text or whitespace-only).
fn extract_pdf_text(path: &std::path::Path) -> Option<String> {
    // pdf-extract::extract_text reads a PDF and returns its text content.
    // It's synchronous and CPU-bound. On failure (corrupt PDF, password-
    // protected, etc.) it returns an Err which we map to None.
    let text = match pdf_extract::extract_text(path) {
        Ok(t) => t,
        Err(_) => return None,
    };
    let trimmed = text.trim();
    // Heuristic: if the extracted text is shorter than 50 chars, it's
    // probably a scanned PDF (we got page numbers and footer text only).
    if trimmed.len() < 50 { return None; }
    Some(trimmed.to_lowercase())
}

/// Compact a string for substring matching: lowercase + collapse whitespace
/// to single spaces. Different from normalize_for_match which strips ALL
/// non-alphanumerics — for PDFs we want to preserve word boundaries because
/// we're searching for things like "GSTIN: 27AAACF5737C1ZV" where the
/// alphanumeric run is the meaningful unit.
fn compact_pdf_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !last_was_ws { out.push(' '); }
            last_was_ws = true;
        } else {
            out.push(c.to_ascii_lowercase());
            last_was_ws = false;
        }
    }
    out
}

/// Cached per-file PDF text. Computed on first need, reused for every
/// contract scoring pass. Returns the compacted lowercase text or None.
struct PdfTextCache<'a> {
    files: &'a mut std::collections::HashMap<String, Option<String>>,
}
impl<'a> PdfTextCache<'a> {
    fn get_or_extract(&mut self, abs_path: &str) -> Option<&str> {
        if !self.files.contains_key(abs_path) {
            let path = std::path::Path::new(abs_path);
            // Only attempt PDF extraction for .pdf files
            let is_pdf = abs_path.to_lowercase().ends_with(".pdf");
            let text = if is_pdf {
                extract_pdf_text(path).map(|s| compact_pdf_text(&s))
            } else {
                None
            };
            self.files.insert(abs_path.to_string(), text);
        }
        self.files.get(abs_path).and_then(|o| o.as_deref())
    }
}

/// Per-client metadata used during matching. Built once from the clients
/// table; passed to scorer for substring checks against PDF body text.
#[derive(Clone)]
struct ClientMatchData {
    /// Display name (key in clients table, e.g. "63 Moons Technologies Limited")
    full_name: String,
    /// Lowercase short name ("63 moons")
    short_name_lc: String,
    /// Lowercase GSTIN (15 chars, e.g. "27aaacf5737c1zv"). Empty if missing.
    gstin_lc: String,
    /// Lowercase PAN (10 chars, e.g. "aaacf5737c"). Empty if missing.
    pan_lc: String,
    /// Lowercase vendor code (used in OEM invoices to identify the client
    /// in the OEM's accounting system).
    vendor_code_lc: String,
    /// Lowercase CIN (corporate identification number).
    cin_lc: String,
}

/// Read client master (ms_client_master_v1) from app_settings or the clients
/// table and build a HashMap: client_name -> ClientMatchData.
fn build_client_match_data(con: &Connection) -> std::collections::HashMap<String, ClientMatchData> {
    let mut out = std::collections::HashMap::new();
    if let Ok(mut stmt) = con.prepare("SELECT name, raw_data FROM clients") {
        if let Ok(rows) = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        }) {
            for row in rows.flatten() {
                let (name, raw) = row;
                let v: serde_json::Value = match serde_json::from_str(&raw) {
                    Ok(v) => v, Err(_) => continue,
                };
                let cmd = ClientMatchData {
                    full_name: name.clone(),
                    short_name_lc: jstr(&v, "shortName").to_lowercase(),
                    gstin_lc: jstr(&v, "gstin").to_lowercase(),
                    pan_lc: jstr(&v, "pan").to_lowercase(),
                    vendor_code_lc: jstr(&v, "vendorCode").to_lowercase(),
                    cin_lc: jstr(&v, "cin").to_lowercase(),
                };
                out.insert(name, cmd);
            }
        }
    }
    out
}

/// Score the *PDF body content* against a contract+field. Returns extra
/// score points and reasons that should be ADDED to the filename-based
/// score. Returns (0.0, []) if no PDF text was extracted.
///
/// Why separate from score_field_match? Two reasons:
///   - PDF text extraction is expensive (100-300ms per file). We only want
///     to do it once per file, then re-use the cached text across all
///     contracts.
///   - PDF signals are stronger than filename signals. They get higher
///     individual weights and need their own scoring rationale in the
///     "Why?" diagnostic.
#[allow(clippy::too_many_arguments)]
fn score_pdf_body(
    pdf_text: Option<&str>,
    contract: &serde_json::Value,
    client_data: Option<&ClientMatchData>,
    field_kind: &FieldKind,
) -> (f64, Vec<String>) {
    let text = match pdf_text {
        Some(t) => t,
        None => return (0.0, Vec::new()),
    };
    let mut score: f64 = 0.0;
    let mut reasons: Vec<String> = Vec::new();

    // ---- GSTIN / PAN match (very strong client identification) ----
    if let Some(cd) = client_data {
        if cd.gstin_lc.len() >= 10 && text.contains(&cd.gstin_lc) {
            score += 0.50;
            reasons.push(format!("GSTIN '{}'", cd.gstin_lc.to_uppercase()));
        } else if cd.pan_lc.len() >= 8 && text.contains(&cd.pan_lc) {
            // PAN is embedded in GSTIN, so don't double-count, but if GSTIN
            // wasn't present we still credit PAN.
            score += 0.40;
            reasons.push(format!("PAN '{}'", cd.pan_lc.to_uppercase()));
        }
        // CIN is rarer but still a strong signal
        if cd.cin_lc.len() >= 10 && text.contains(&cd.cin_lc) {
            score += 0.20;
            reasons.push(format!("CIN '{}'", cd.cin_lc.to_uppercase()));
        }
        // Vendor code (used by OEMs to identify the client in their accounting)
        // Only credit for OEM invoices; a numeric code is too noisy elsewhere.
        if matches!(field_kind, FieldKind::OemInvoice(_))
            && cd.vendor_code_lc.len() >= 4
            && text.contains(&cd.vendor_code_lc) {
            score += 0.18;
            reasons.push(format!("vendor code '{}'", cd.vendor_code_lc));
        }

        // Full client name in body (more reliable than filename match)
        let full_lc = cd.full_name.to_lowercase();
        if !full_lc.is_empty() && text.contains(&full_lc) {
            score += 0.15;
            reasons.push("client name in body".to_string());
        } else if !cd.short_name_lc.is_empty()
            && cd.short_name_lc.len() >= 4
            && text.contains(&cd.short_name_lc) {
            score += 0.10;
            reasons.push(format!("client short '{}' in body", cd.short_name_lc));
        }
    }

    // ---- Doc number in body (the strongest possible signal) ----
    let doc_number = match field_kind {
        FieldKind::InternalPO => jstr(contract, "internalPO"),
        FieldKind::ClientPOLegacy => jstr(contract, "clientPO"),
        FieldKind::ClientPOArr(po_idx) => {
            contract.get("clientPOs").and_then(|v| v.as_array())
                .and_then(|a| a.get(*po_idx))
                .map(|po| jstr(po, "poNo")).unwrap_or_default()
        }
        FieldKind::ClientInvoice(y_idx) => {
            contract.get("billingYears").and_then(|v| v.as_array())
                .and_then(|a| a.get(*y_idx))
                .map(|by| jstr(by, "clientInvoiceNo")).unwrap_or_default()
        }
        FieldKind::OemInvoice(y_idx) => {
            contract.get("billingYears").and_then(|v| v.as_array())
                .and_then(|a| a.get(*y_idx))
                .map(|by| jstr(by, "oemInvoiceNo")).unwrap_or_default()
        }
    };
    if !doc_number.is_empty() {
        let dn_lc = doc_number.to_lowercase();
        if dn_lc.len() >= 4 && text.contains(&dn_lc) {
            score += 0.70;
            reasons.push(format!("doc# '{doc_number}' in body"));
        } else {
            // Try alternative forms — separators stripped, or just the suffix
            // (e.g. "21-22/DT102" → also try "21-22dt102" and "DT102").
            let stripped = dn_lc.replace(['-', '/', ' ', '_'], "");
            if stripped.len() >= 4 && text.replace([' ', '-', '_', '/'], "").contains(&stripped) {
                score += 0.65;
                reasons.push(format!("doc# '{doc_number}' (compact match)"));
            } else {
                let parts: Vec<&str> = dn_lc.split(['/', '-', '_', ' ']).filter(|s| !s.is_empty()).collect();
                for part in parts.iter().rev() {
                    if part.len() >= 4 && text.contains(*part) {
                        score += 0.50;
                        reasons.push(format!("doc# part '{part}' in body"));
                        break;
                    }
                }
            }
        }
    }

    // ---- Date in body matching the record date (within ±2 days) ----
    let record_date = match field_kind {
        FieldKind::InternalPO => jstr(contract, "internalPODate"),
        FieldKind::ClientPOLegacy => jstr(contract, "clientPODate"),
        FieldKind::ClientPOArr(po_idx) => {
            contract.get("clientPOs").and_then(|v| v.as_array())
                .and_then(|a| a.get(*po_idx))
                .map(|po| jstr(po, "poDate")).unwrap_or_default()
        }
        FieldKind::ClientInvoice(y_idx) => {
            contract.get("billingYears").and_then(|v| v.as_array())
                .and_then(|a| a.get(*y_idx)).map(|by| {
                    let d = jstr(by, "actualInvoiceDate");
                    if !d.is_empty() { d } else { jstr(by, "billDate") }
                }).unwrap_or_default()
        }
        FieldKind::OemInvoice(y_idx) => {
            contract.get("billingYears").and_then(|v| v.as_array())
                .and_then(|a| a.get(*y_idx)).map(|by| {
                    let d = jstr(by, "oemInvoiceDate");
                    if !d.is_empty() { d } else { jstr(by, "billDate") }
                }).unwrap_or_default()
        }
    };
    if record_date.len() >= 10 {
        // Build several common formats to look for.
        let y = &record_date[0..4];
        let m: i32 = record_date[5..7].parse().unwrap_or(0);
        let d: i32 = record_date[8..10].parse().unwrap_or(0);
        let month_names_short = ["jan","feb","mar","apr","may","jun","jul","aug","sep","oct","nov","dec"];
        let month_names_long = ["january","february","march","april","may","june","july","august","september","october","november","december"];
        let mname_short = if (1..=12).contains(&m) { month_names_short[(m-1) as usize] } else { "" };
        let mname_long = if (1..=12).contains(&m) { month_names_long[(m-1) as usize] } else { "" };

        let cands = [
            format!("{:02}/{:02}/{}", d, m, y),     // 04/04/2025
            format!("{:02}-{:02}-{}", d, m, y),     // 04-04-2025
            format!("{:02}.{:02}.{}", d, m, y),     // 04.04.2025
            format!("{}/{:02}/{:02}", y, m, d),     // 2025/04/04
            format!("{}-{:02}-{:02}", y, m, d),     // 2025-04-04
            format!("{} {} {}", d, mname_short, y), // 4 apr 2025
            format!("{} {} {}", d, mname_long, y),  // 4 april 2025
            format!("{:02} {} {}", d, mname_short, y), // 04 apr 2025
            format!("{:02} {} {}", d, mname_long, y),  // 04 april 2025
            format!("{}{}{}", d, mname_short, y),
            format!("{} {}, {}", mname_long, d, y), // april 4, 2025
        ];
        let mut date_matched = false;
        for c in &cands {
            let cl = c.to_lowercase();
            if !cl.is_empty() && text.contains(&cl) {
                score += 0.25;
                reasons.push(format!("date '{c}' in body"));
                date_matched = true;
                break;
            }
        }
        if !date_matched {
            // Fall back: at least the year + month name
            if !mname_short.is_empty() {
                let probe = format!("{mname_short} {y}");
                if text.contains(&probe) {
                    score += 0.10;
                    reasons.push(format!("month/year '{probe}' in body"));
                }
            }
        }
    }

    (score, reasons)
}

/// Build a minimal MatchCandidate from contract+field, with confidence 0.0
/// and no reasons. Used when filename scoring rejected the file but PDF
/// body content gave a strong match (e.g. filename has zero overlap but
/// body contains GSTIN + doc number).
fn build_bare_candidate(
    contract: &serde_json::Value,
    contract_idx: usize,
    field_kind: &FieldKind,
) -> Option<MatchCandidate> {
    let (record_date, doc_number, doc_type, category_segs, original_url) = match field_kind {
        FieldKind::InternalPO => {
            let url = jstr(contract, "internalPOLink");
            if url.is_empty() { return None; }
            (jstr(contract, "internalPODate"), jstr(contract, "internalPO"),
             "Internal PO".to_string(),
             vec!["POs".to_string(), "Internal".to_string()], url)
        }
        FieldKind::ClientPOLegacy => {
            let url = jstr(contract, "clientPOLink");
            if url.is_empty() { return None; }
            (jstr(contract, "clientPODate"), jstr(contract, "clientPO"),
             "Client PO".to_string(),
             vec!["POs".to_string(), "Client".to_string()], url)
        }
        FieldKind::ClientPOArr(po_idx) => {
            let arr = contract.get("clientPOs").and_then(|v| v.as_array());
            let po = arr.and_then(|a| a.get(*po_idx))?;
            let url = jstr(po, "poLink");
            if url.is_empty() { return None; }
            (jstr(po, "poDate"), jstr(po, "poNo"),
             "Client PO".to_string(),
             vec!["POs".to_string(), "Client".to_string()], url)
        }
        FieldKind::ClientInvoice(y_idx) => {
            let arr = contract.get("billingYears").and_then(|v| v.as_array());
            let by = arr.and_then(|a| a.get(*y_idx))?;
            let url = jstr(by, "clientInvoiceLink");
            if url.is_empty() { return None; }
            let dt = jstr(by, "actualInvoiceDate");
            let dt = if !dt.is_empty() { dt } else { jstr(by, "billDate") };
            (dt, jstr(by, "clientInvoiceNo"),
             "Client Invoice".to_string(),
             vec!["Sales Invoices".to_string()], url)
        }
        FieldKind::OemInvoice(y_idx) => {
            let arr = contract.get("billingYears").and_then(|v| v.as_array());
            let by = arr.and_then(|a| a.get(*y_idx))?;
            let url = jstr(by, "oemInvoiceLink");
            if url.is_empty() { return None; }
            let dt = jstr(by, "oemInvoiceDate");
            let dt = if !dt.is_empty() { dt } else { jstr(by, "billDate") };
            (dt, jstr(by, "oemInvoiceNo"),
             "OEM Invoice".to_string(),
             vec!["Purchase Invoices".to_string()], url)
        }
    };

    let client = jstr(contract, "client");
    let product = jstr(contract, "product");
    let label = if !doc_number.is_empty() {
        format!("{client} — {product} — {doc_type} #{doc_number} ({record_date})")
    } else {
        format!("{client} — {product} — {doc_type} ({record_date})")
    };
    let suggested_fy = if !record_date.is_empty() { date_to_fy(&record_date) } else { None };

    let field_path = match field_kind {
        FieldKind::InternalPO => format!("contracts[{contract_idx}].internalPOLink"),
        FieldKind::ClientPOLegacy => format!("contracts[{contract_idx}].clientPOLink"),
        FieldKind::ClientPOArr(po_idx) => format!("contracts[{contract_idx}].clientPOs[{po_idx}].poLink"),
        FieldKind::ClientInvoice(y_idx) => format!("contracts[{contract_idx}].billingYears[{y_idx}].clientInvoiceLink"),
        FieldKind::OemInvoice(y_idx) => format!("contracts[{contract_idx}].billingYears[{y_idx}].oemInvoiceLink"),
    };

    Some(MatchCandidate {
        field_path, label, confidence: 0.0, reasons: Vec::new(),
        original_url, category_path: category_segs, suggested_fy,
        document_type: doc_type,
    })
}

#[allow(clippy::too_many_arguments)]
fn score_field_match(
    filename: &str,
    filename_lc: &str,
    filename_norm: &str,
    file_date: Option<&str>,
    contract: &serde_json::Value,
    contract_idx: usize,
    field_kind: &FieldKind,
    client_full_norm: &str,
    client_short_norm: &str,
    client_tokens: &[String],
    product: &str,
    oem_aliases: &[(String, String)],
    product_code_aliases: &[(String, Vec<String>)],
    contract_oem: &str,
) -> Option<MatchCandidate> {
    let _ = filename;       // currently unused; reserved for fuzzy comparisons
    let _ = filename_lc;    // currently unused

    let mut score: f64 = 0.0;
    let mut reasons: Vec<String> = Vec::new();

    // ---- Client name match (strong signal) ----
    let mut client_matched = false;
    if !client_full_norm.is_empty() && filename_norm.contains(client_full_norm) {
        score += 0.45;
        reasons.push("full client name".to_string());
        client_matched = true;
    } else if client_short_norm.len() >= 3 && filename_norm.contains(client_short_norm) {
        score += 0.40;
        reasons.push(format!("short client name '{}'", client_short_norm));
        client_matched = true;
    } else {
        // Token-based partial match (e.g. "63 Moons" matches "63MOONS")
        for tok in client_tokens {
            let norm_tok = normalize_for_match(tok);
            if norm_tok.len() >= 3 && filename_norm.contains(&norm_tok) {
                score += 0.30;
                reasons.push(format!("client token '{}'", tok));
                client_matched = true;
                break;
            }
        }
    }

    // ---- OEM match ----
    let mut oem_matched_name = String::new();
    // First look for OEM short codes (DT, VRS, etc.) as standalone underscore-bounded
    // tokens in the filename to avoid "DT" appearing inside "DTPRODUCT".
    for (alias, display) in oem_aliases {
        let needle_norm = normalize_for_match(alias);
        if needle_norm.is_empty() { continue; }
        // For short codes (<= 4 chars), require a word boundary in original filename
        // to avoid coincidental substring matches.
        if alias.len() <= 4 {
            let lc = filename_lc;
            if has_token(lc, alias) {
                score += 0.20;
                reasons.push(format!("OEM alias '{alias}' → {display}"));
                oem_matched_name = display.clone();
                break;
            }
        } else if filename_norm.contains(&needle_norm) {
            score += 0.20;
            reasons.push(format!("OEM '{display}'"));
            oem_matched_name = display.clone();
            break;
        }
    }

    // ---- Product / product-code match ----
    let mut product_matched = false;
    let product_norm = normalize_for_match(product);
    let product_inner: String = {
        // Pull bracketed text "Darktrace (DT Email)" → "DT Email"
        let mut inner = String::new();
        if let (Some(o), Some(c)) = (product.find('('), product.find(')')) {
            if o < c {
                inner = product[o+1..c].to_string();
            }
        }
        inner
    };
    let product_inner_norm = normalize_for_match(&product_inner);

    if product_inner_norm.len() >= 3 && filename_norm.contains(&product_inner_norm) {
        score += 0.18;
        reasons.push(format!("product '{}'", product_inner));
        product_matched = true;
    } else if product_norm.len() >= 5 && filename_norm.contains(&product_norm) {
        score += 0.18;
        reasons.push(format!("product '{}'", product));
        product_matched = true;
    }
    // Product-code aliases (AGE → "DT Email", etc.)
    if !product_matched {
        for (code, products) in product_code_aliases {
            // Code matches as a word boundary in filename
            if has_token(filename_lc, code) {
                // Does this contract's product match any in the alias's product list?
                let pn_lc = product.to_lowercase();
                for ap in products {
                    let ap_norm = normalize_for_match(ap);
                    if pn_lc.contains(&ap.to_lowercase())
                        || product_inner_norm == ap_norm
                        || product_norm.contains(&ap_norm) {
                        score += 0.22;
                        reasons.push(format!("product code '{code}' → '{ap}'"));
                        product_matched = true;
                        break;
                    }
                }
                if product_matched { break; }
            }
        }
    }

    // ---- Field-specific extraction ----
    let (record_date, doc_number, doc_type, category_segs, original_url) = match field_kind {
        FieldKind::InternalPO => {
            let num = jstr(contract, "internalPO");
            let dt = jstr(contract, "internalPODate");
            let url = jstr(contract, "internalPOLink");
            (dt, num, "Internal PO".to_string(),
             vec!["POs".to_string(), "Internal".to_string()], url)
        }
        FieldKind::ClientPOLegacy => {
            let num = jstr(contract, "clientPO");
            let dt = jstr(contract, "clientPODate");
            let url = jstr(contract, "clientPOLink");
            (dt, num, "Client PO".to_string(),
             vec!["POs".to_string(), "Client".to_string()], url)
        }
        FieldKind::ClientPOArr(po_idx) => {
            let arr = contract.get("clientPOs").and_then(|v| v.as_array());
            let po = arr.and_then(|a| a.get(*po_idx));
            let num = po.map(|p| jstr(p, "poNo")).unwrap_or_default();
            let dt = po.map(|p| jstr(p, "poDate")).unwrap_or_default();
            let url = po.map(|p| jstr(p, "poLink")).unwrap_or_default();
            (dt, num, "Client PO".to_string(),
             vec!["POs".to_string(), "Client".to_string()], url)
        }
        FieldKind::ClientInvoice(y_idx) => {
            let arr = contract.get("billingYears").and_then(|v| v.as_array());
            let by = arr.and_then(|a| a.get(*y_idx));
            let num = by.map(|b| jstr(b, "clientInvoiceNo")).unwrap_or_default();
            let dt = by.map(|b| {
                let d = jstr(b, "actualInvoiceDate");
                if !d.is_empty() { d } else { jstr(b, "billDate") }
            }).unwrap_or_default();
            let url = by.map(|b| jstr(b, "clientInvoiceLink")).unwrap_or_default();
            (dt, num, "Client Invoice".to_string(),
             vec!["Sales Invoices".to_string()], url)
        }
        FieldKind::OemInvoice(y_idx) => {
            let arr = contract.get("billingYears").and_then(|v| v.as_array());
            let by = arr.and_then(|a| a.get(*y_idx));
            let num = by.map(|b| jstr(b, "oemInvoiceNo")).unwrap_or_default();
            let dt = by.map(|b| {
                let d = jstr(b, "oemInvoiceDate");
                if !d.is_empty() { d } else { jstr(b, "billDate") }
            }).unwrap_or_default();
            let url = by.map(|b| jstr(b, "oemInvoiceLink")).unwrap_or_default();
            (dt, num, "OEM Invoice".to_string(),
             vec!["Purchase Invoices".to_string()], url)
        }
    };

    if original_url.is_empty() { return None; }

    // ---- Doc number match (very strong if found) ----
    if !doc_number.is_empty() {
        let doc_norm = normalize_for_match(&doc_number);
        if doc_norm.len() >= 4 && filename_norm.contains(&doc_norm) {
            score += 0.55;
            reasons.push(format!("doc# '{doc_number}'"));
        } else if doc_norm.len() >= 4 {
            // Try with the original separators replaced by underscores
            let alt = doc_number.replace(['/', '-', ' '], "_");
            let alt_norm = normalize_for_match(&alt);
            if alt_norm == doc_norm && filename_lc.contains(&alt.to_lowercase()) {
                // already handled above via norm
            }
            // Also try a substring of the doc number after the year prefix
            // e.g. "23-24/04/DT161" → try matching just "DT161" (5 chars+)
            let parts: Vec<&str> = doc_number.split(['/', '-', '_', ' ']).filter(|s| !s.is_empty()).collect();
            for part in parts.iter().rev() {
                let part_norm = normalize_for_match(part);
                if part_norm.len() >= 4 && filename_norm.contains(&part_norm) {
                    score += 0.45;
                    reasons.push(format!("doc# part '{part}'"));
                    break;
                }
            }
        }
    }

    // ---- Date match ----
    let mut date_matched = false;
    if let (Some(fd), rd) = (file_date, record_date.as_str()) {
        if !rd.is_empty() {
            let fd_n = date_to_day_number(fd);
            let rd_n = date_to_day_number(rd);
            if let (Some(a), Some(b)) = (fd_n, rd_n) {
                let diff = (a - b).abs();
                if diff == 0 {
                    score += 0.30;
                    reasons.push("exact date".to_string());
                    date_matched = true;
                } else if diff <= 2 {
                    score += 0.22;
                    reasons.push(format!("date ±{diff} day(s)"));
                    date_matched = true;
                } else if diff <= 7 {
                    score += 0.10;
                    reasons.push(format!("date ±{diff} days (week)"));
                    date_matched = true;
                } else if fd[..7] == rd[..7] {
                    // Same year-month
                    score += 0.06;
                    reasons.push("same month".to_string());
                    date_matched = true;
                } else if fd[..4] == rd[..4] {
                    // Same year
                    score += 0.03;
                    reasons.push("same year".to_string());
                    date_matched = true;
                }
            }
        }
    }
    // If filename has any year matching a contract year (without record_date)
    if !date_matched {
        if let Some(fd) = file_date {
            if fd.len() >= 4 && filename.contains(&fd[..4]) {
                // Already counted via filename_lc parsing — small bonus only
                score += 0.04;
                reasons.push(format!("year {}", &fd[..4]));
            }
        }
    }

    // ---- For OEM Invoice: missing OEM name in filename is a negative ----
    if matches!(field_kind, FieldKind::OemInvoice(_)) && oem_matched_name.is_empty() {
        // If contract has known OEM and filename doesn't reference it, dampen.
        if !contract_oem.is_empty() {
            score *= 0.65;
        }
    }

    // ---- Hard floor: if no client AND no OEM, this is noise ----
    if !client_matched && oem_matched_name.is_empty() {
        return None;
    }

    if score < 0.05 { return None; }

    let final_score = score.min(1.0);

    // Build human-readable label
    let client = jstr(contract, "client");
    let label = if !doc_number.is_empty() {
        format!("{client} — {product} — {doc_type} #{doc_number} ({record_date})")
    } else {
        format!("{client} — {product} — {doc_type} ({record_date})")
    };

    let suggested_fy = if !record_date.is_empty() {
        date_to_fy(&record_date)
    } else {
        // Fall back to file date
        file_date.and_then(date_to_fy)
    };

    let field_path = match field_kind {
        FieldKind::InternalPO => format!("contracts[{contract_idx}].internalPOLink"),
        FieldKind::ClientPOLegacy => format!("contracts[{contract_idx}].clientPOLink"),
        FieldKind::ClientPOArr(po_idx) => format!("contracts[{contract_idx}].clientPOs[{po_idx}].poLink"),
        FieldKind::ClientInvoice(y_idx) => format!("contracts[{contract_idx}].billingYears[{y_idx}].clientInvoiceLink"),
        FieldKind::OemInvoice(y_idx) => format!("contracts[{contract_idx}].billingYears[{y_idx}].oemInvoiceLink"),
    };

    Some(MatchCandidate {
        field_path, label, confidence: final_score, reasons,
        original_url, category_path: category_segs, suggested_fy,
        document_type: doc_type,
    })
}

/// Returns true if the filename (lowercased) contains the token bounded by
/// non-alphanumeric characters (or string boundary). Used for short codes
/// like "dt" that would otherwise match coincidental substrings.
fn has_token(haystack_lc: &str, needle: &str) -> bool {
    let needle_lc = needle.to_lowercase();
    if needle_lc.is_empty() { return false; }
    let bytes = haystack_lc.as_bytes();
    let nbytes = needle_lc.as_bytes();
    let mut i = 0;
    while i + nbytes.len() <= bytes.len() {
        if &bytes[i..i + nbytes.len()] == nbytes {
            let before_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
            let after_ok = i + nbytes.len() == bytes.len()
                || !bytes[i + nbytes.len()].is_ascii_alphanumeric();
            if before_ok && after_ok { return true; }
        }
        i += 1;
    }
    false
}

/// Convenience: read a string field, returning empty if missing.
fn jstr(v: &serde_json::Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).map(String::from).unwrap_or_default()
}

/// Walk migration folder.
fn list_migration_files() -> Result<Vec<MigrationFile>, String> {
    let root = migration_root().ok_or("Could not resolve migration folder")?;
    if !root.exists() {
        return Err("Migration folder does not exist. Initialize first.".into());
    }
    let mut out: Vec<MigrationFile> = Vec::new();
    let mut stack: Vec<PathBuf> = vec![root];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e, Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else { continue; };
            if name == ".DS_Store" || name.starts_with("._") { continue; }
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => stack.push(path),
                Ok(ft) if ft.is_file() => {
                    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    let filename = name.to_string();
                    let filename_lc = filename.to_lowercase();
                    out.push(MigrationFile {
                        abs_path: path.to_string_lossy().to_string(),
                        filename, filename_lc, size_bytes: size,
                    });
                }
                _ => {}
            }
        }
    }
    Ok(out)
}

#[derive(Serialize)]
struct ScanResult {
    ok: bool,
    error: Option<String>,
    file_count: usize,
}

#[tauri::command]
fn scan_migration_folder() -> ScanResult {
    match list_migration_files() {
        Ok(files) => ScanResult { ok: true, error: None, file_count: files.len() },
        Err(e) => ScanResult { ok: false, error: Some(e), file_count: 0 },
    }
}

#[derive(Serialize)]
struct ProposeResult {
    ok: bool,
    error: Option<String>,
    files: Vec<FileWithMatches>,
    auto_count: usize,
    review_count: usize,
    unmatched_count: usize,
    total_files: usize,
}

// Build B-fix-4: thresholds adjusted for PDF-augmented scoring.
// Auto raised to 0.95 — we have higher-quality signals (GSTIN, doc#-in-body,
// dates-in-body), so higher confidence required for silent auto-apply.
const AUTO_THRESHOLD: f64 = 0.95;
const REVIEW_THRESHOLD: f64 = 0.20;

#[tauri::command]
fn propose_file_matches() -> ProposeResult {
    // 1. Read contracts + clients + OEMs from SQLite.
    let dbp = match db_path() {
        Some(p) => p, None => return ProposeResult {
            ok: false, error: Some("no db path".into()),
            files: vec![], auto_count: 0, review_count: 0, unmatched_count: 0, total_files: 0,
        },
    };
    let con = match Connection::open_with_flags(&dbp, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(c) => c, Err(e) => return ProposeResult {
            ok: false, error: Some(format!("open db: {e}")),
            files: vec![], auto_count: 0, review_count: 0, unmatched_count: 0, total_files: 0,
        },
    };

    let mut contracts: Vec<serde_json::Value> = Vec::new();
    {
        let mut stmt = match con.prepare("SELECT raw_data FROM contracts ORDER BY legacy_idx ASC") {
            Ok(s) => s, Err(e) => return ProposeResult {
                ok: false, error: Some(format!("prepare contracts: {e}")),
                files: vec![], auto_count: 0, review_count: 0, unmatched_count: 0, total_files: 0,
            },
        };
        let rows = match stmt.query_map([], |r| r.get::<_, String>(0)) {
            Ok(r) => r, Err(e) => return ProposeResult {
                ok: false, error: Some(format!("query contracts: {e}")),
                files: vec![], auto_count: 0, review_count: 0, unmatched_count: 0, total_files: 0,
            },
        };
        for row in rows.flatten() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&row) {
                contracts.push(v);
            }
        }
    }

    // Client master — for short names AND full match data (Build B-fix-4)
    let mut client_short_map: BTreeMap<String, String> = BTreeMap::new();
    let client_match_data = build_client_match_data(&con);
    for (name, cmd) in &client_match_data {
        if !cmd.short_name_lc.is_empty() {
            client_short_map.insert(name.clone(), cmd.short_name_lc.clone());
        }
    }

    let oem_aliases = load_oem_aliases();
    let product_code_aliases = load_product_code_aliases();

    // 2. List files.
    let files = match list_migration_files() {
        Ok(f) => f, Err(e) => return ProposeResult {
            ok: false, error: Some(e),
            files: vec![], auto_count: 0, review_count: 0, unmatched_count: 0, total_files: 0,
        },
    };

    // 3. For each file, score against every contract field.
    //    Per-file PDF text is extracted on first need and cached, so each
    //    file is parsed at most once even though it's compared against
    //    many contracts.
    let mut pdf_cache: std::collections::HashMap<String, Option<String>> =
        std::collections::HashMap::new();
    let mut results: Vec<FileWithMatches> = Vec::new();

    for f in &files {
        let filename_norm = normalize_for_match(&f.filename_lc);
        let file_date = extract_filename_date(&f.filename);
        let file_date_ref: Option<&str> = file_date.as_deref();

        // Extract PDF text once for this file.
        let pdf_text: Option<String> = {
            let mut cache = PdfTextCache { files: &mut pdf_cache };
            cache.get_or_extract(&f.abs_path).map(String::from)
        };
        let pdf_text_ref: Option<&str> = pdf_text.as_deref();

        let mut candidates: Vec<MatchCandidate> = Vec::new();

        for (idx, contract) in contracts.iter().enumerate() {
            let client = jstr(contract, "client");
            let client_norm = normalize_for_match(&client);
            let client_short = client_short_map.get(&client).cloned().unwrap_or_default();
            let client_short_norm = normalize_for_match(&client_short);
            let client_toks = name_tokens(&client);

            let product = jstr(contract, "product");

            // OEM name from product field "Darktrace (DT Email)" -> "Darktrace"
            let contract_oem = if let Some(o) = product.find('(') {
                product[..o].trim().to_string()
            } else { String::new() };

            // Collect all fields with non-empty links
            let mut kinds: Vec<FieldKind> = Vec::new();
            if !jstr(contract, "internalPOLink").is_empty() {
                kinds.push(FieldKind::InternalPO);
            }
            if !jstr(contract, "clientPOLink").is_empty() {
                kinds.push(FieldKind::ClientPOLegacy);
            }
            if let Some(arr) = contract.get("clientPOs").and_then(|v| v.as_array()) {
                for (po_idx, po) in arr.iter().enumerate() {
                    if !jstr(po, "poLink").is_empty() {
                        kinds.push(FieldKind::ClientPOArr(po_idx));
                    }
                }
            }
            if let Some(arr) = contract.get("billingYears").and_then(|v| v.as_array()) {
                for (y_idx, by) in arr.iter().enumerate() {
                    if !jstr(by, "clientInvoiceLink").is_empty() {
                        kinds.push(FieldKind::ClientInvoice(y_idx));
                    }
                    if !jstr(by, "oemInvoiceLink").is_empty() {
                        kinds.push(FieldKind::OemInvoice(y_idx));
                    }
                }
            }

            // Look up client match data for PDF scoring (used below)
            let client_data_for_pdf = client_match_data.get(&client);

            for k in &kinds {
                // Filename-based scoring (existing behavior)
                let mut maybe_cand = score_field_match(
                    &f.filename, &f.filename_lc, &filename_norm, file_date_ref,
                    contract, idx, k,
                    &client_norm, &client_short_norm, &client_toks,
                    &product, &oem_aliases, &product_code_aliases,
                    &contract_oem,
                );

                // PDF body scoring layered on top, if PDF text was extractable.
                // Even if filename scoring rejected this candidate, a strong
                // PDF body match can revive it (e.g. filename has no info but
                // body has GSTIN + doc number + dates).
                let (pdf_score, pdf_reasons) = score_pdf_body(
                    pdf_text_ref, contract, client_data_for_pdf, k,
                );

                if pdf_score >= 0.40 && maybe_cand.is_none() {
                    // Strong PDF match but filename scorer rejected. Build a
                    // bare candidate and add the PDF score.
                    maybe_cand = build_bare_candidate(contract, idx, k);
                }

                if let Some(mut c) = maybe_cand {
                    c.confidence = (c.confidence + pdf_score).min(1.0);
                    for r in pdf_reasons { c.reasons.push(r); }
                    candidates.push(c);
                }
            }
        }

        // Dedupe candidates that share the same field_path
        let mut seen = BTreeMap::new();
        for c in candidates.into_iter() {
            seen.entry(c.field_path.clone())
                .and_modify(|existing: &mut MatchCandidate| {
                    if c.confidence > existing.confidence { *existing = c.clone(); }
                })
                .or_insert(c);
        }
        let mut deduped: Vec<MatchCandidate> = seen.into_values().collect();

        // Sort by confidence descending
        deduped.sort_by(|a, b| b.confidence.partial_cmp(&a.confidence).unwrap_or(std::cmp::Ordering::Equal));
        deduped.truncate(8);

        let bucket = if deduped.is_empty() {
            "unmatched".to_string()
        } else if deduped[0].confidence >= AUTO_THRESHOLD {
            "auto".to_string()
        } else if deduped[0].confidence >= REVIEW_THRESHOLD {
            "review".to_string()
        } else {
            "unmatched".to_string()
        };

        results.push(FileWithMatches { file: f.clone(), candidates: deduped, bucket });
    }

    let total = results.len();
    let auto_count = results.iter().filter(|r| r.bucket == "auto").count();
    let review_count = results.iter().filter(|r| r.bucket == "review").count();
    let unmatched_count = results.iter().filter(|r| r.bucket == "unmatched").count();

    ProposeResult {
        ok: true, error: None,
        files: results,
        auto_count, review_count, unmatched_count,
        total_files: total,
    }
}

/// Sanitize a filename. Drops chars unsafe on macOS, collapses whitespace,
/// trims length.
fn safe_filename_part_v2(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_space = false;
    for c in s.chars() {
        let safe = match c {
            '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*' | '\0' => '_',
            _ => c,
        };
        if safe == ' ' || safe == '\t' {
            if !last_was_space { out.push(' '); last_was_space = true; }
        } else {
            out.push(safe);
            last_was_space = false;
        }
    }
    let trimmed = out.trim().to_string();
    if trimmed.len() > 100 { trimmed.chars().take(100).collect() } else { trimmed }
}

#[derive(Serialize)]
struct ApplyResult {
    ok: bool,
    error: Option<String>,
    destination: Option<String>,
    record_updated: bool,
}

#[tauri::command]
fn apply_file_assignment(
    file_path: String,
    action: String,
    fy: Option<String>,
    category_path: Option<Vec<String>>,
    field_path: Option<String>,
) -> ApplyResult {
    if action == "skip" {
        return ApplyResult { ok: true, error: None, destination: None, record_updated: false };
    }
    if action != "link_to_record" && action != "file_only" {
        return ApplyResult {
            ok: false, error: Some(format!("Unknown action: {action}")),
            destination: None, record_updated: false,
        };
    }
    let src = PathBuf::from(&file_path);
    if !src.exists() {
        return ApplyResult {
            ok: false, error: Some("Source file not found.".into()),
            destination: None, record_updated: false,
        };
    }
    let fy_str = match fy.as_ref() {
        Some(s) if !s.is_empty() => s.clone(),
        _ => return ApplyResult {
            ok: false, error: Some("FY is required.".into()),
            destination: None, record_updated: false,
        },
    };
    if !FY_FOLDERS.iter().any(|f| **f == fy_str) {
        return ApplyResult {
            ok: false, error: Some(format!("Unknown FY: {fy_str}")),
            destination: None, record_updated: false,
        };
    }
    let cat = match category_path.as_ref() {
        Some(c) if !c.is_empty() => c.clone(),
        _ => return ApplyResult {
            ok: false, error: Some("Category path is required.".into()),
            destination: None, record_updated: false,
        },
    };
    for seg in &cat {
        if let Err(e) = validate_path_segment(seg) {
            return ApplyResult {
                ok: false, error: Some(format!("Invalid category segment: {e}")),
                destination: None, record_updated: false,
            };
        }
    }
    let files = match files_root() {
        Some(p) => p, None => return ApplyResult {
            ok: false, error: Some("Could not resolve Files folder.".into()),
            destination: None, record_updated: false,
        },
    };
    // Build B-fix-2: Layout is Files/{category-path}/{FY}/file.pdf
    let mut dest_dir = files.clone();
    for seg in &cat { dest_dir.push(seg); }
    dest_dir.push(&fy_str);
    if let Err(e) = std::fs::create_dir_all(&dest_dir) {
        return ApplyResult {
            ok: false, error: Some(format!("Could not create destination: {e}")),
            destination: None, record_updated: false,
        };
    }
    let fname = src.file_name().and_then(|s| s.to_str())
        .map(|s| safe_filename_part_v2(s))
        .unwrap_or_else(|| "file".to_string());
    let mut dest = dest_dir.join(&fname);
    let mut counter = 1;
    while dest.exists() {
        let (stem, ext) = match fname.rsplit_once('.') {
            Some((s, e)) => (s.to_string(), format!(".{e}")),
            None => (fname.clone(), String::new()),
        };
        dest = dest_dir.join(format!("{stem} ({counter}){ext}"));
        counter += 1;
        if counter > 999 {
            return ApplyResult {
                ok: false, error: Some("Too many filename collisions.".into()),
                destination: None, record_updated: false,
            };
        }
    }
    match std::fs::rename(&src, &dest) {
        Ok(_) => {}
        Err(_) => {
            if let Err(e) = std::fs::copy(&src, &dest) {
                return ApplyResult {
                    ok: false, error: Some(format!("Could not move file: {e}")),
                    destination: None, record_updated: false,
                };
            }
            if let Err(e) = std::fs::remove_file(&src) {
                return ApplyResult {
                    ok: false,
                    error: Some(format!("File copied but original could not be removed: {e}")),
                    destination: Some(dest.to_string_lossy().to_string()),
                    record_updated: false,
                };
            }
        }
    }
    let mut record_updated = false;
    if action == "link_to_record" {
        let fp = match field_path.as_ref() {
            Some(s) if !s.is_empty() => s.clone(),
            _ => return ApplyResult {
                ok: true,
                error: Some("File moved but no field_path provided.".into()),
                destination: Some(dest.to_string_lossy().to_string()),
                record_updated: false,
            },
        };
        match update_contract_local_field(&fp, &dest.to_string_lossy().to_string()) {
            Ok(()) => { record_updated = true; }
            Err(e) => return ApplyResult {
                ok: true,
                error: Some(format!("File moved but record update failed: {e}")),
                destination: Some(dest.to_string_lossy().to_string()),
                record_updated: false,
            },
        }
    }
    ApplyResult {
        ok: true, error: None,
        destination: Some(dest.to_string_lossy().to_string()),
        record_updated,
    }
}

fn update_contract_local_field(field_path: &str, local_path: &str) -> Result<(), String> {
    let stripped = field_path.trim_start_matches("contracts[");
    let close_idx = stripped.find(']').ok_or("malformed field path")?;
    let idx_str = &stripped[..close_idx];
    let idx: i64 = idx_str.parse().map_err(|e| format!("idx parse: {e}"))?;
    let rest = &stripped[close_idx + 2..];

    let dbp = db_path().ok_or("no db path")?;
    let mut con = Connection::open_with_flags(&dbp, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .map_err(|e| format!("open: {e}"))?;
    let _: String = con.query_row("PRAGMA journal_mode=WAL;", [], |r| r.get(0))
        .map_err(|e| format!("wal: {e}"))?;
    let raw: String = con.query_row(
        "SELECT raw_data FROM contracts WHERE legacy_idx = ?1",
        params![idx], |r| r.get(0),
    ).map_err(|e| format!("read contract {idx}: {e}"))?;
    let mut data: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| format!("parse contract: {e}"))?;
    apply_local_at_subpath(&mut data, rest, local_path)?;
    let new_raw = data.to_string();
    con.execute(
        "UPDATE contracts SET raw_data = ?1, modified_at = ?2 WHERE legacy_idx = ?3",
        params![new_raw, now_iso(), idx],
    ).map_err(|e| format!("update: {e}"))?;
    Ok(())
}

fn apply_local_at_subpath(
    data: &mut serde_json::Value,
    subpath: &str,
    local_path: &str,
) -> Result<(), String> {
    let bytes = subpath.as_bytes();
    let mut i = 0usize;
    let mut current: &mut serde_json::Value = data;
    loop {
        let start = i;
        while i < bytes.len() {
            let c = bytes[i];
            if (c as char).is_alphanumeric() || c == b'_' { i += 1; } else { break; }
        }
        let ident = std::str::from_utf8(&bytes[start..i]).map_err(|e| format!("utf8: {e}"))?;
        if ident.is_empty() { return Err(format!("expected identifier at {start}")); }
        let at_end = i >= bytes.len();
        let next_is_bracket = !at_end && bytes[i] == b'[';
        let next_is_dot = !at_end && bytes[i] == b'.';

        if at_end {
            let parent_obj = current.as_object_mut().ok_or("expected object at leaf")?;
            let local_field = format!("{ident}Local");
            parent_obj.insert(local_field, serde_json::Value::String(local_path.to_string()));
            return Ok(());
        }
        if next_is_dot {
            let next = current.get_mut(ident).ok_or_else(|| format!("missing field '{ident}'"))?;
            current = next;
            i += 1;
            continue;
        }
        if next_is_bracket {
            i += 1;
            let idx_start = i;
            while i < bytes.len() && bytes[i] != b']' { i += 1; }
            if i >= bytes.len() { return Err("missing ']'".into()); }
            let idx_str = std::str::from_utf8(&bytes[idx_start..i]).map_err(|e| format!("utf8: {e}"))?;
            let arr_idx: usize = idx_str.parse().map_err(|e| format!("index parse: {e}"))?;
            i += 1;
            let next = current.get_mut(ident).ok_or_else(|| format!("missing field '{ident}'"))?;
            let arr = next.as_array_mut().ok_or_else(|| format!("expected array at '{ident}'"))?;
            let elem = arr.get_mut(arr_idx).ok_or_else(|| format!("array '{ident}' has no index {arr_idx}"))?;
            current = elem;
            if i >= bytes.len() { return Err("subpath ends after array index".into()); }
            if bytes[i] == b'.' { i += 1; } else { return Err("expected '.' after array index".into()); }
            continue;
        }
        return Err(format!("unexpected char in subpath at {i}"));
    }
}

#[tauri::command]
fn open_local_file<R: tauri::Runtime>(app: tauri::AppHandle<R>, path: String) -> serde_json::Value {
    if path.is_empty() {
        return serde_json::json!({"ok": false, "error": "Empty path"});
    }
    let pb = PathBuf::from(&path);
    if !pb.exists() {
        return serde_json::json!({"ok": false, "error": "File no longer exists at that path."});
    }
    match app.opener().open_path(path.clone(), None::<&str>) {
        Ok(_) => serde_json::json!({"ok": true, "path": path}),
        Err(e) => serde_json::json!({"ok": false, "error": format!("{e}")}),
    }
}

/// Returns ALL contract link fields. The JS side decides whether to
/// filter by `already_has_local`. Includes a flag so the picker can
/// optionally show fields that already have a local file (for re-linking).
#[tauri::command]
fn list_linkable_record_fields() -> serde_json::Value {
    let dbp = match db_path() {
        Some(p) => p,
        None => return serde_json::json!({"ok": false, "error": "no db path"}),
    };
    let con = match Connection::open_with_flags(&dbp, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(c) => c, Err(e) => return serde_json::json!({"ok": false, "error": format!("open: {e}")}),
    };
    let mut stmt = match con.prepare("SELECT legacy_idx, raw_data FROM contracts ORDER BY legacy_idx ASC") {
        Ok(s) => s, Err(e) => return serde_json::json!({"ok": false, "error": format!("prepare: {e}")}),
    };
    let rows = match stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))) {
        Ok(r) => r, Err(e) => return serde_json::json!({"ok": false, "error": format!("query: {e}")}),
    };

    let mut entries: Vec<serde_json::Value> = Vec::new();
    for row in rows.flatten() {
        let idx = row.0 as usize;
        let v: serde_json::Value = match serde_json::from_str(&row.1) {
            Ok(v) => v, Err(_) => continue,
        };
        let client = jstr(&v, "client");
        let product = jstr(&v, "product");

        let push = |entries: &mut Vec<serde_json::Value>, fp: String, label: String, has_local: bool, current_local: String| {
            entries.push(serde_json::json!({
                "field_path": fp,
                "label": label,
                "already_has_local": has_local,
                "current_local": current_local,
            }));
        };

        if !jstr(&v, "internalPOLink").is_empty() {
            let local = jstr(&v, "internalPOLinkLocal");
            push(&mut entries,
                format!("contracts[{idx}].internalPOLink"),
                format!("{client} — {product} — Internal PO"),
                !local.is_empty(), local);
        }
        if !jstr(&v, "clientPOLink").is_empty() {
            let local = jstr(&v, "clientPOLinkLocal");
            push(&mut entries,
                format!("contracts[{idx}].clientPOLink"),
                format!("{client} — {product} — Client PO (legacy)"),
                !local.is_empty(), local);
        }
        if let Some(arr) = v.get("clientPOs").and_then(|x| x.as_array()) {
            for (po_idx, po) in arr.iter().enumerate() {
                if !jstr(po, "poLink").is_empty() {
                    let local = jstr(po, "poLinkLocal");
                    let no = jstr(po, "poNo");
                    push(&mut entries,
                        format!("contracts[{idx}].clientPOs[{po_idx}].poLink"),
                        format!("{client} — {product} — Client PO #{no}"),
                        !local.is_empty(), local);
                }
            }
        }
        if let Some(arr) = v.get("billingYears").and_then(|x| x.as_array()) {
            for (y_idx, by) in arr.iter().enumerate() {
                if !jstr(by, "clientInvoiceLink").is_empty() {
                    let local = jstr(by, "clientInvoiceLinkLocal");
                    let no = jstr(by, "clientInvoiceNo");
                    push(&mut entries,
                        format!("contracts[{idx}].billingYears[{y_idx}].clientInvoiceLink"),
                        format!("{client} — {product} — Client Invoice #{no} (Y{y_idx})"),
                        !local.is_empty(), local);
                }
                if !jstr(by, "oemInvoiceLink").is_empty() {
                    let local = jstr(by, "oemInvoiceLinkLocal");
                    let no = jstr(by, "oemInvoiceNo");
                    push(&mut entries,
                        format!("contracts[{idx}].billingYears[{y_idx}].oemInvoiceLink"),
                        format!("{client} — {product} — OEM Invoice #{no} (Y{y_idx})"),
                        !local.is_empty(), local);
                }
            }
        }
    }
    serde_json::json!({"ok": true, "fields": entries})
}

/// Diagnostic: count how many fields currently have a _local set.
/// Useful for UI dashboards and debugging.
#[tauri::command]
fn get_local_field_status() -> serde_json::Value {
    let dbp = match db_path() {
        Some(p) => p,
        None => return serde_json::json!({"ok": false, "error": "no db path"}),
    };
    let con = match Connection::open_with_flags(&dbp, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(c) => c, Err(e) => return serde_json::json!({"ok": false, "error": format!("open: {e}")}),
    };
    let mut stmt = match con.prepare("SELECT raw_data FROM contracts ORDER BY legacy_idx ASC") {
        Ok(s) => s, Err(e) => return serde_json::json!({"ok": false, "error": format!("prepare: {e}")}),
    };
    let rows = match stmt.query_map([], |r| r.get::<_, String>(0)) {
        Ok(r) => r, Err(e) => return serde_json::json!({"ok": false, "error": format!("query: {e}")}),
    };

    let mut total_fields: usize = 0;
    let mut with_local: usize = 0;
    for row in rows.flatten() {
        let v: serde_json::Value = match serde_json::from_str(&row) {
            Ok(v) => v, Err(_) => continue,
        };
        // Count link fields and their _local siblings
        for k in ["internalPOLink", "clientPOLink"] {
            if !jstr(&v, k).is_empty() {
                total_fields += 1;
                if !jstr(&v, &format!("{k}Local")).is_empty() { with_local += 1; }
            }
        }
        if let Some(arr) = v.get("clientPOs").and_then(|x| x.as_array()) {
            for po in arr {
                if !jstr(po, "poLink").is_empty() {
                    total_fields += 1;
                    if !jstr(po, "poLinkLocal").is_empty() { with_local += 1; }
                }
            }
        }
        if let Some(arr) = v.get("billingYears").and_then(|x| x.as_array()) {
            for by in arr {
                for k in ["clientInvoiceLink", "oemInvoiceLink"] {
                    if !jstr(by, k).is_empty() {
                        total_fields += 1;
                        if !jstr(by, &format!("{k}Local")).is_empty() { with_local += 1; }
                    }
                }
            }
        }
    }
    serde_json::json!({
        "ok": true,
        "total_fields": total_fields,
        "with_local": with_local,
        "without_local": total_fields - with_local,
    })
}

/// Save user-defined product-code aliases. `aliases` is an array of
/// {"code": "...", "products": ["..."]} objects.
#[tauri::command]
fn save_product_code_aliases(state: tauri::State<DbState>, aliases: serde_json::Value) -> SaveResult {
    let json = match serde_json::to_string(&aliases) {
        Ok(s) => s, Err(e) => return SaveResult::err(format!("serialize: {e}")),
    };
    match with_writer(&state, |con| {
        con.execute(
            "INSERT INTO app_settings (key, value, modified_at)
             VALUES ('ms_app__product_code_aliases', ?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, modified_at = excluded.modified_at",
            params![json, now_iso()],
        )?;
        Ok(1usize)
    }) {
        Ok(n) => SaveResult::ok(n),
        Err(e) => SaveResult::err(e),
    }
}

/// Save user-defined OEM aliases. `aliases` is an object mapping short code
/// to canonical OEM display name. e.g. {"DT": "Darktrace"}.
#[tauri::command]
fn save_oem_aliases(state: tauri::State<DbState>, aliases: serde_json::Value) -> SaveResult {
    let json = match serde_json::to_string(&aliases) {
        Ok(s) => s, Err(e) => return SaveResult::err(format!("serialize: {e}")),
    };
    match with_writer(&state, |con| {
        con.execute(
            "INSERT INTO app_settings (key, value, modified_at)
             VALUES ('ms_app__oem_aliases', ?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, modified_at = excluded.modified_at",
            params![json, now_iso()],
        )?;
        Ok(1usize)
    }) {
        Ok(n) => SaveResult::ok(n),
        Err(e) => SaveResult::err(e),
    }
}

/// Read product-code aliases for the Settings UI.
#[tauri::command]
fn get_product_code_aliases() -> serde_json::Value {
    let aliases = load_product_code_aliases();
    let arr: Vec<serde_json::Value> = aliases.into_iter().map(|(code, products)| {
        serde_json::json!({"code": code.to_uppercase(), "products": products})
    }).collect();
    serde_json::json!({"ok": true, "aliases": arr})
}

/// Read OEM aliases for the Settings UI.
#[tauri::command]
fn get_oem_aliases() -> serde_json::Value {
    let aliases = load_oem_aliases();
    let mut obj = serde_json::Map::new();
    for (k, v) in aliases {
        obj.insert(k.to_uppercase(), serde_json::Value::String(v));
    }
    serde_json::json!({"ok": true, "aliases": obj})
}


// ============================================================================
// Build B-fix-2: Folder hierarchy auto-migration
//
// Earlier builds put files at Files/{FY}/{category}/file.pdf.
// Build B-fix-2 inverts this to Files/{category}/{FY}/file.pdf.
// This command walks the existing Files/ tree, moves every file to its new
// location, and updates every contract _local field that pointed to the
// old path. Idempotent — re-running is safe.
//
// Old → new category-segment translation:
//   Invoices/Client/  →  Sales Invoices/
//   Invoices/OEM/     →  Purchase Invoices/
//   POs/Internal/     →  POs/Internal/    (unchanged)
//   POs/Client/       →  POs/Client/      (unchanged)
//   Proposals/        →  Proposals/       (unchanged)
//   <other>           →  <other>          (preserved verbatim)
// ============================================================================

#[derive(Serialize)]
struct RearrangeResult {
    ok: bool,
    error: Option<String>,
    moved: usize,
    updated_records: usize,
    skipped_already_correct: usize,
    skipped_unrecognized: usize,
    errors: Vec<String>,
}

/// Translate a category-segment list from the OLD layout to the NEW layout.
/// Returns None if the segments don't look like a known category (skipped).
fn translate_category_segments(old_segs: &[String]) -> Option<Vec<String>> {
    if old_segs.is_empty() { return None; }
    match old_segs.first().map(|s| s.as_str()) {
        Some("Invoices") => {
            match old_segs.get(1).map(|s| s.as_str()) {
                Some("Client") => Some(vec!["Sales Invoices".to_string()]),
                Some("OEM") => Some(vec!["Purchase Invoices".to_string()]),
                _ => None, // Unknown Invoices subfolder
            }
        }
        Some("POs") => {
            // POs/Internal and POs/Client unchanged
            match old_segs.get(1).map(|s| s.as_str()) {
                Some("Internal") | Some("Client") => Some(old_segs.to_vec()),
                _ => None,
            }
        }
        Some("Proposals") => Some(vec!["Proposals".to_string()]),
        _ => {
            // User-added custom category — keep verbatim
            Some(old_segs.to_vec())
        }
    }
}

/// Returns true if a path looks like a FY folder (e.g. "FY24-25").
fn is_fy_folder(name: &str) -> bool {
    FY_FOLDERS.iter().any(|f| **f == *name)
}

#[tauri::command]
fn rearrange_files_to_new_structure(state: tauri::State<DbState>) -> RearrangeResult {
    let mut result = RearrangeResult {
        ok: true, error: None, moved: 0, updated_records: 0,
        skipped_already_correct: 0, skipped_unrecognized: 0,
        errors: Vec::new(),
    };

    let files = match files_root() {
        Some(p) => p,
        None => {
            result.ok = false;
            result.error = Some("Could not resolve Files root.".into());
            return result;
        }
    };
    if !files.exists() {
        result.ok = false;
        result.error = Some("Files folder does not exist. Initialize first.".into());
        return result;
    }

    // Collect all files first (snapshot — don't iterate while moving)
    let mut all_files: Vec<PathBuf> = Vec::new();
    let mut stack: Vec<PathBuf> = vec![files.clone()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e, Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else { continue; };
            if name == ".DS_Store" || name.starts_with("._") { continue; }
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => stack.push(path),
                Ok(ft) if ft.is_file() => all_files.push(path),
                _ => {}
            }
        }
    }

    // For each file, parse its current path under Files/ and determine if it
    // needs to move. Build a list of (src, dst) pairs.
    let mut moves: Vec<(PathBuf, PathBuf, Vec<String>, String)> = Vec::new();
    // ^ (src_abs, dst_abs, new_category_segs, fy)

    for f in &all_files {
        let rel = match f.strip_prefix(&files) {
            Ok(r) => r, Err(_) => { result.skipped_unrecognized += 1; continue; }
        };
        let segs: Vec<String> = rel.iter()
            .filter_map(|os| os.to_str().map(String::from))
            .collect();
        // Need at least: <fy>/<cat-seg1>/...<file>
        if segs.len() < 3 {
            result.skipped_unrecognized += 1; continue;
        }
        let first = segs[0].as_str();

        // Detect already-new layout: first is a category, last-but-one is a FY
        if !is_fy_folder(first) {
            // Already in new layout? Last-but-one segment should be a FY.
            let last_but_one = &segs[segs.len() - 2];
            if is_fy_folder(last_but_one) {
                // Already in new structure. Verify category translation gives
                // same path; if so, skip.
                let cat_segs: Vec<String> = segs[..segs.len()-2].to_vec();
                // Translate (in case it's old "Invoices/Client" already at new layer)
                if let Some(new_cat) = translate_category_segments(&cat_segs) {
                    if new_cat == cat_segs {
                        result.skipped_already_correct += 1;
                        continue;
                    }
                    // Old name in new layout — translate
                    let mut dst = files.clone();
                    for s in &new_cat { dst.push(s); }
                    dst.push(last_but_one);
                    dst.push(&segs[segs.len()-1]);
                    if dst == *f {
                        result.skipped_already_correct += 1;
                        continue;
                    }
                    moves.push((f.clone(), dst, new_cat, last_but_one.clone()));
                    continue;
                } else {
                    result.skipped_already_correct += 1;
                    continue;
                }
            }
            result.skipped_unrecognized += 1;
            continue;
        }

        // Old layout: segs[0] = FY, segs[1..len-1] = category, segs[len-1] = filename
        let fy = segs[0].clone();
        let filename = segs[segs.len() - 1].clone();
        let old_cat: Vec<String> = segs[1..segs.len() - 1].to_vec();
        if old_cat.is_empty() {
            // File directly under FY folder, no category — skip
            result.skipped_unrecognized += 1;
            continue;
        }

        // Translate
        let new_cat = match translate_category_segments(&old_cat) {
            Some(c) => c,
            None => { result.skipped_unrecognized += 1; continue; }
        };

        let mut dst = files.clone();
        for s in &new_cat { dst.push(s); }
        dst.push(&fy);
        dst.push(&filename);

        if dst == *f {
            result.skipped_already_correct += 1;
            continue;
        }
        moves.push((f.clone(), dst, new_cat, fy));
    }

    // Build a mapping of old absolute path → new absolute path for record updates.
    let mut path_map: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    for (src, dst, _new_cat, _fy) in &moves {
        // Make sure parent exists.
        if let Some(parent) = dst.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                result.errors.push(format!("mkdir {}: {}", parent.display(), e));
                continue;
            }
        }
        // If destination already exists, append number.
        let mut final_dst = dst.clone();
        let mut counter = 1;
        while final_dst.exists() {
            let stem_ext = match final_dst.file_name().and_then(|s| s.to_str()) {
                Some(n) => match n.rsplit_once('.') {
                    Some((s, e)) => (s.to_string(), format!(".{e}")),
                    None => (n.to_string(), String::new()),
                },
                None => break,
            };
            if let Some(parent) = dst.parent() {
                final_dst = parent.join(format!("{} ({}){}", stem_ext.0, counter, stem_ext.1));
            }
            counter += 1;
            if counter > 999 {
                result.errors.push(format!("Too many collisions for {}", dst.display()));
                break;
            }
        }
        if counter > 999 { continue; }

        match std::fs::rename(src, &final_dst) {
            Ok(_) => {}
            Err(_) => {
                if let Err(e) = std::fs::copy(src, &final_dst) {
                    result.errors.push(format!("copy {}: {}", src.display(), e));
                    continue;
                }
                if let Err(e) = std::fs::remove_file(src) {
                    result.errors.push(format!("rm orig {}: {}", src.display(), e));
                    continue;
                }
            }
        }
        result.moved += 1;
        path_map.insert(
            src.to_string_lossy().to_string(),
            final_dst.to_string_lossy().to_string(),
        );
    }

    // Update _local fields in every contract that points to a moved file.
    if !path_map.is_empty() {
        match update_local_paths_in_contracts(&state, &path_map) {
            Ok(n) => result.updated_records = n,
            Err(e) => result.errors.push(format!("contract update: {e}")),
        }
    }

    // Mark migration done in app_settings (idempotent — won't break re-runs).
    let _ = with_writer(&state, |con| {
        con.execute(
            "INSERT INTO app_settings (key, value, modified_at)
             VALUES ('ms_app__files_rearranged_v1', '1', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, modified_at = excluded.modified_at",
            params![now_iso()],
        )?;
        Ok(1usize)
    });

    // Try to clean up now-empty old FY directories at the top level.
    if let Ok(entries) = std::fs::read_dir(&files) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                    if is_fy_folder(name) {
                        prune_empty_dirs(&path);
                    }
                }
            }
        }
    }

    result
}

/// Recursively delete empty directories, walking back up.
fn prune_empty_dirs(start: &PathBuf) {
    if !start.is_dir() { return; }
    // First recurse into children
    if let Ok(entries) = std::fs::read_dir(start) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() { prune_empty_dirs(&p); }
        }
    }
    // Then try removing this if empty
    let _ = std::fs::remove_dir(start); // fails silently if not empty
}

/// Walk every contract and update any _local field whose value matches a key
/// in path_map, replacing it with the corresponding new value. Returns the
/// number of contracts whose data was rewritten.
fn update_local_paths_in_contracts(
    state: &tauri::State<DbState>,
    path_map: &std::collections::HashMap<String, String>,
) -> Result<usize, String> {
    with_writer(state, |con| {
        // Collect all contract rows first
        let mut to_update: Vec<(i64, String)> = Vec::new();
        {
            let mut stmt = con.prepare("SELECT legacy_idx, raw_data FROM contracts ORDER BY legacy_idx ASC")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
            for row in rows.flatten() {
                let (idx, raw) = row;
                if let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&raw) {
                    let mut changed = false;
                    rewrite_local_paths(&mut v, path_map, &mut changed);
                    if changed {
                        to_update.push((idx, v.to_string()));
                    }
                }
            }
        }
        let n = to_update.len();
        for (idx, new_raw) in to_update {
            con.execute(
                "UPDATE contracts SET raw_data = ?1, modified_at = ?2 WHERE legacy_idx = ?3",
                params![new_raw, now_iso(), idx],
            )?;
        }
        Ok(n)
    })
}

/// Recursively walk a JSON value, replacing any string ending with
/// "Local" → key whose value matches a key in path_map.
fn rewrite_local_paths(
    v: &mut serde_json::Value,
    path_map: &std::collections::HashMap<String, String>,
    changed: &mut bool,
) {
    match v {
        serde_json::Value::Object(obj) => {
            // Mutate in place: walk keys, find *Local string values
            let keys: Vec<String> = obj.keys().cloned().collect();
            for k in keys {
                if k.ends_with("Local") {
                    if let Some(s) = obj.get(&k).and_then(|x| x.as_str()) {
                        if let Some(new) = path_map.get(s) {
                            obj.insert(k.clone(), serde_json::Value::String(new.clone()));
                            *changed = true;
                        }
                    }
                } else {
                    // Recurse into nested objects/arrays
                    if let Some(child) = obj.get_mut(&k) {
                        rewrite_local_paths(child, path_map, changed);
                    }
                }
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr.iter_mut() {
                rewrite_local_paths(item, path_map, changed);
            }
        }
        _ => {}
    }
}


// ============================================================================
// Build B-fix-4: Reset all assignments
//
// Undoes everything that prior auto-applies and manual links did:
//   - Strips every *Local field from every contract record
//   - Moves every file currently under Files/{category}/{FY}/ back to
//     Migration/ (and from the old layout Files/{FY}/{category}/ too,
//     for safety)
//   - Removes the rearrange-completed flag so re-running the rearrange
//     button later still works
//   - Empty Files/ subdirectories are pruned
//
// After this runs, the user can re-scan with the new PDF reader and start
// fresh. Idempotent — re-running on an already-reset state is a no-op.
// ============================================================================

#[derive(Serialize)]
struct ResetResult {
    ok: bool,
    error: Option<String>,
    records_reset: usize,
    files_moved_back: usize,
    errors: Vec<String>,
}

#[tauri::command]
fn reset_all_assignments(state: tauri::State<DbState>) -> ResetResult {
    let mut result = ResetResult {
        ok: true, error: None,
        records_reset: 0, files_moved_back: 0,
        errors: Vec::new(),
    };

    // 1. Walk every file under Files/ and move back to Migration/.
    let files = match files_root() {
        Some(p) => p,
        None => {
            result.ok = false;
            result.error = Some("Could not resolve Files root.".into());
            return result;
        }
    };
    let migration = match migration_root() {
        Some(p) => p,
        None => {
            result.ok = false;
            result.error = Some("Could not resolve Migration root.".into());
            return result;
        }
    };

    if !files.exists() {
        // Nothing to move; just clear records and return.
    } else {
        if !migration.exists() {
            if let Err(e) = std::fs::create_dir_all(&migration) {
                result.errors.push(format!("create Migration: {e}"));
            }
        }
        let mut stack: Vec<PathBuf> = vec![files.clone()];
        let mut all_files: Vec<PathBuf> = Vec::new();
        while let Some(dir) = stack.pop() {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    let Some(name) = path.file_name().and_then(|s| s.to_str()) else { continue; };
                    if name == ".DS_Store" || name.starts_with("._") { continue; }
                    match entry.file_type() {
                        Ok(ft) if ft.is_dir() => stack.push(path),
                        Ok(ft) if ft.is_file() => all_files.push(path),
                        _ => {}
                    }
                }
            }
        }
        for src in &all_files {
            let fname = src.file_name().and_then(|s| s.to_str()).unwrap_or("file");
            let mut dest = migration.join(fname);
            // Avoid collision
            let mut counter = 1;
            while dest.exists() {
                let (stem, ext) = match fname.rsplit_once('.') {
                    Some((s, e)) => (s.to_string(), format!(".{e}")),
                    None => (fname.to_string(), String::new()),
                };
                dest = migration.join(format!("{stem} ({counter}){ext}"));
                counter += 1;
                if counter > 999 {
                    result.errors.push(format!("Too many collisions for {fname}"));
                    break;
                }
            }
            if counter > 999 { continue; }
            match std::fs::rename(src, &dest) {
                Ok(_) => { result.files_moved_back += 1; }
                Err(_) => {
                    if let Err(e) = std::fs::copy(src, &dest) {
                        result.errors.push(format!("copy {}: {}", src.display(), e));
                        continue;
                    }
                    if let Err(e) = std::fs::remove_file(src) {
                        result.errors.push(format!("rm {}: {}", src.display(), e));
                        continue;
                    }
                    result.files_moved_back += 1;
                }
            }
        }
        // Prune empty subdirs of Files/
        if let Ok(entries) = std::fs::read_dir(&files) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() { prune_empty_dirs(&path); }
            }
        }
    }

    // 2. Strip *Local fields from every contract.
    match strip_all_local_fields_from_contracts(&state) {
        Ok(n) => result.records_reset = n,
        Err(e) => {
            result.errors.push(format!("strip locals: {e}"));
            result.ok = false;
        }
    }

    // 3. Clear the rearrange-completed flag so a future re-arrange can run.
    let _ = with_writer(&state, |con| {
        con.execute(
            "DELETE FROM app_settings WHERE key = 'ms_app__files_rearranged_v1'",
            [],
        )?;
        Ok(1usize)
    });

    result
}

/// Strip every field whose name ends in "Local" from every contract's
/// raw_data. Returns the number of contracts whose data was rewritten.
fn strip_all_local_fields_from_contracts(state: &tauri::State<DbState>) -> Result<usize, String> {
    with_writer(state, |con| {
        let mut to_update: Vec<(i64, String)> = Vec::new();
        {
            let mut stmt = con.prepare("SELECT legacy_idx, raw_data FROM contracts ORDER BY legacy_idx ASC")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
            for row in rows.flatten() {
                let (idx, raw) = row;
                if let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&raw) {
                    let mut changed = false;
                    strip_local_keys_in_place(&mut v, &mut changed);
                    if changed {
                        to_update.push((idx, v.to_string()));
                    }
                }
            }
        }
        let n = to_update.len();
        for (idx, new_raw) in to_update {
            con.execute(
                "UPDATE contracts SET raw_data = ?1, modified_at = ?2 WHERE legacy_idx = ?3",
                params![new_raw, now_iso(), idx],
            )?;
        }
        Ok(n)
    })
}

/// Recursively walk a JSON value, removing any object key ending in "Local".
fn strip_local_keys_in_place(v: &mut serde_json::Value, changed: &mut bool) {
    match v {
        serde_json::Value::Object(obj) => {
            // Collect keys to remove first to avoid mutation-during-iteration.
            let to_remove: Vec<String> = obj.keys()
                .filter(|k| k.ends_with("Local"))
                .cloned().collect();
            for k in to_remove {
                obj.remove(&k);
                *changed = true;
            }
            // Recurse into remaining values
            for (_, child) in obj.iter_mut() {
                strip_local_keys_in_place(child, changed);
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr.iter_mut() {
                strip_local_keys_in_place(item, changed);
            }
        }
        _ => {}
    }
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
            // Session 6:
            get_app_settings,
            save_app_setting,
            pick_folder,
            run_backup_check,
            check_db_conflict,
            acknowledge_db_conflict,
            extract_attachments,
            load_attachments_into_data_urls,
            // Session 8:
            initialize_files_folders,
            get_files_setup_status,
            get_document_categories,
            add_document_category,
            remove_document_category,
            reveal_path,
            // Session 8 Build B (matcher) — improved:
            scan_migration_folder,
            propose_file_matches,
            apply_file_assignment,
            list_linkable_record_fields,
            open_local_file,
            get_local_field_status,
            get_product_code_aliases,
            save_product_code_aliases,
            get_oem_aliases,
            save_oem_aliases,
            // Build B-fix-2: folder structure auto-migration
            rearrange_files_to_new_structure,
            // Build B-fix-4: PDF reader + reset
            reset_all_assignments,
        ])
        .setup(|_app| Ok(()))
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
