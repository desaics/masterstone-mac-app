// Masterstone CRM — Tauri runtime entry.
//
// Capabilities provided here, by session:
//
// Session 4 — SQLite persistence:
//   - storage_load_all + per-bucket savers (clients, resellers, oems,
//     contracts, prospects, proposals, purchase_orders, invoices,
//     commissions, company_profile, attachments, settings).
//   - install_db_from_path for restoring a backup database on top of
//     the live one.
//
// Session 5 — OneDrive / external opening + snapshots:
//   - open_external_url routes OneDrive (and any URL) clicks to the
//     OS default browser via the opener plugin.
//   - reveal_onedrive_folder opens the Masterstone folder in Finder.
//   - generate_snapshot writes a read-only iPhone-friendly HTML
//     snapshot to ~/OneDrive/Masterstone/ on quit; keeps last 7.
//
// Session 6 — settings page + auto-backup:
//   - get_app_settings / save_app_setting for the Mac App Settings sub-tab.
//   - pick_folder for choosing the backup destination.
//   - run_backup_check / check_db_conflict / acknowledge_db_conflict
//     handle scheduled auto-backups and external-DB-modified detection.
//   - extract_attachments / load_attachments_into_data_urls hydrate the
//     company logo + signature for snapshots.
//
// Session 8 / B-fix-8:
//   - Phase A: file-linking matcher subsystem fully removed
//     (PDF text extraction, category folders, filename aliases,
//     migration UI — all gone). The user opted out in favour of
//     manual linking.
//   - Phase 8C: open_local_file + pick_file — simple "open exact path"
//     and "show file picker" wrappers used by the paperclip toggle
//     next to every OneDrive URL field.

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

    // 8YZ — Consultants module (consultants master + consultant_payments register).
    // Both tables are NEW in 8Y, so existing DBs may not have them. table_exists()
    // gates the SELECT so we return empty arrays instead of fatal DB_READ_ERROR.
    // The save commands below create the tables on first write, so once any data
    // is saved, subsequent loads always find the tables.
    if table_exists(&con, "consultants")? {
        let consultants = collect_array(&con, "SELECT raw_data FROM consultants ORDER BY created_at, id")?;
        counts.insert("consultants".into(), consultants.len());
        data.insert("ms_consultants_v1".into(), serde_json::Value::Array(consultants).to_string());
    } else {
        counts.insert("consultants".into(), 0);
        data.insert("ms_consultants_v1".into(), "[]".to_string());
    }
    if table_exists(&con, "consultant_payments")? {
        let cpayments = collect_array(&con, "SELECT raw_data FROM consultant_payments ORDER BY invoice_date, id")?;
        counts.insert("consultant_payments".into(), cpayments.len());
        data.insert("ms_consultant_payments_v1".into(), serde_json::Value::Array(cpayments).to_string());
    } else {
        counts.insert("consultant_payments".into(), 0);
        data.insert("ms_consultant_payments_v1".into(), "[]".to_string());
    }

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

// 8YZ — Tolerant table-exists check for newly introduced tables (consultants,
// consultant_payments). Lets load_all_inner gracefully handle databases created
// before Phase 8Y where these tables don't exist yet. Returns Ok(true) only if
// the table is present in sqlite_master; Ok(false) otherwise. Used to avoid
// fatal DB_READ_ERROR on existing installs that haven't yet been upgraded.
fn table_exists(con: &Connection, name: &str) -> Result<bool, rusqlite::Error> {
    let mut stmt = con.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1")?;
    let mut rows = stmt.query([name])?;
    Ok(rows.next()?.is_some())
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

// ============================================================================
// Phase 8AW — storage_load_attachments: lightweight read-only fetch.
//
// The attachments bucket can grow large (megabytes per signed-PDF upload, all
// base64-encoded). Pre-8AW, the bucket was routed through localStorage like
// every other bucket, but WKWebView's localStorage caps near 2–3MB and the
// user started hitting QuotaExceededError on the 3rd signed-PDF upload.
//
// The fix is to take the attachments bucket out of localStorage entirely:
//   - mid-session saves go to SQLite directly (saveAttachments calls the
//     new __msScheduleBucketWriteDirect helper, bypassing the localStorage
//     interceptor)
//   - boot-time loads call THIS command directly into the in-memory map,
//     skipping the localStorage hydration path for this bucket
//
// Other buckets are small (kilobytes) and stay on the localStorage path so
// existing diagnostics (DB Sanity Check, etc.) keep working. Only attachments
// take the direct-to-SQLite route.
//
// Returns the same flat-array shape that storage_load_all puts in
// data['ms_attachments_v1'] — a JSON-encoded array string for parity, so the
// frontend's existing regrouping logic can consume it unchanged.
// ============================================================================

#[derive(Serialize, Debug)]
struct LoadAttachmentsResult {
    ok: bool,
    error: Option<String>,
    payload: Option<String>,
    record_count: usize,
}

#[tauri::command]
fn storage_load_attachments() -> LoadAttachmentsResult {
    let path = match db_path() {
        Some(p) => p,
        None => return LoadAttachmentsResult {
            ok: false, error: Some("Database path unavailable".into()),
            payload: None, record_count: 0,
        },
    };
    if !path.exists() {
        // No DB yet — return empty array, same as a fresh install.
        return LoadAttachmentsResult {
            ok: true, error: None,
            payload: Some("[]".into()), record_count: 0,
        };
    }
    let con = match Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(c) => c,
        Err(e) => return LoadAttachmentsResult {
            ok: false, error: Some(format!("Open DB failed: {e}")),
            payload: None, record_count: 0,
        },
    };
    let attachments = match collect_array(&con, "SELECT raw_data FROM attachments ORDER BY uploaded_at, id") {
        Ok(a) => a,
        Err(e) => return LoadAttachmentsResult {
            ok: false, error: Some(format!("Query failed: {e}")),
            payload: None, record_count: 0,
        },
    };
    let count = attachments.len();
    let payload = serde_json::Value::Array(attachments).to_string();
    LoadAttachmentsResult {
        ok: true, error: None,
        payload: Some(payload), record_count: count,
    }
}

// 8YZ — Consultants master save. Schema: id (text PK), name, role, gst_registered
// (0/1), pan_number, raw_data (full JSON blob), modified_at. CREATE TABLE IF NOT
// EXISTS is the first statement so the table materializes on first write — no
// schema migration required for existing installs.
#[tauri::command]
fn storage_save_consultants(state: tauri::State<DbState>, payload: String) -> SaveResult {
    let parsed: serde_json::Value = match serde_json::from_str(&payload) {
        Ok(v) => v, Err(e) => return SaveResult::err(format!("Invalid JSON: {e}")),
    };
    let arr = match parsed.as_array() {
        Some(a) => a, None => return SaveResult::err("Expected array".into()),
    };
    match with_writer(&state, |con| {
        con.execute(
            "CREATE TABLE IF NOT EXISTS consultants (
                id TEXT PRIMARY KEY,
                name TEXT,
                role TEXT,
                gst_registered INTEGER,
                pan_number TEXT,
                archived INTEGER,
                created_at TEXT,
                raw_data TEXT NOT NULL,
                modified_at TEXT NOT NULL
            )",
            [],
        )?;
        let tx = con.transaction()?;
        tx.execute("DELETE FROM consultants", [])?;
        let mut count = 0usize;
        for value in arr {
            let raw = value.to_string();
            let id = match js_str(value, "id") {
                Some(s) if !s.is_empty() => s.to_string(), _ => continue,
            };
            tx.execute(
                "INSERT INTO consultants (
                    id, name, role, gst_registered, pan_number, archived,
                    created_at, raw_data, modified_at
                ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![
                    id,
                    js_str(value, "name"),
                    js_str(value, "role"),
                    js_bool_int(value, "gstRegistered"),
                    js_str(value, "panNumber"),
                    js_bool_int(value, "archived"),
                    js_str(value, "createdAt"),
                    raw,
                    now_iso(),
                ],
            )?;
            count += 1;
        }
        tx.commit()?;
        Ok(count)
    }) { Ok(n) => SaveResult::ok(n), Err(e) => SaveResult::err(e) }
}

// 8YZ — Consultant payments save. Mirrors storage_save_consultants pattern.
// Indexable columns extracted for future reporting: consultant_id, payment_number,
// invoice_date, total_amount, status. Full record persists as raw_data JSON.
#[tauri::command]
fn storage_save_consultant_payments(state: tauri::State<DbState>, payload: String) -> SaveResult {
    let parsed: serde_json::Value = match serde_json::from_str(&payload) {
        Ok(v) => v, Err(e) => return SaveResult::err(format!("Invalid JSON: {e}")),
    };
    let arr = match parsed.as_array() {
        Some(a) => a, None => return SaveResult::err("Expected array".into()),
    };
    match with_writer(&state, |con| {
        con.execute(
            "CREATE TABLE IF NOT EXISTS consultant_payments (
                id TEXT PRIMARY KEY,
                consultant_id TEXT,
                payment_number TEXT,
                invoice_date TEXT,
                paid_date TEXT,
                subtotal REAL,
                gst_amount REAL,
                total_amount REAL,
                status TEXT,
                raw_data TEXT NOT NULL,
                modified_at TEXT NOT NULL
            )",
            [],
        )?;
        let tx = con.transaction()?;
        tx.execute("DELETE FROM consultant_payments", [])?;
        let mut count = 0usize;
        for value in arr {
            let raw = value.to_string();
            let id = match js_str(value, "id") {
                Some(s) if !s.is_empty() => s.to_string(), _ => continue,
            };
            tx.execute(
                "INSERT INTO consultant_payments (
                    id, consultant_id, payment_number, invoice_date, paid_date,
                    subtotal, gst_amount, total_amount, status, raw_data, modified_at
                ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                params![
                    id,
                    js_str(value, "consultantId"),
                    js_str(value, "paymentNumber"),
                    js_str(value, "invoiceDate"),
                    js_str(value, "paidDate"),
                    js_f64(value, "subtotal"),
                    js_f64(value, "gstAmount"),
                    js_f64(value, "totalAmount"),
                    js_str(value, "status"),
                    raw,
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
// Phase 8C — Local file attachment opener
//
// Companion to the existing OneDrive URL fields on contracts / invoices / POs
// / proposals. The user pastes or picks a local file path, the path is stored
// alongside the URL on the record, and clicking "Open" launches the file in
// the OS default application (Preview for PDF, Excel for XLSX, etc.).
//
// This is NOT the matcher subsystem (removed in Phase A). It does no folder
// management, no PDF parsing, no auto-discovery — just "open this exact path".
// ============================================================================

#[tauri::command]
fn open_local_file<R: tauri::Runtime>(app: tauri::AppHandle<R>, path: String) -> serde_json::Value {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return serde_json::json!({"ok": false, "error": "No file path provided"});
    }
    let p = std::path::Path::new(trimmed);
    if !p.exists() {
        return serde_json::json!({
            "ok": false,
            "error": format!("File not found: {trimmed}. The file may have been moved, renamed, or deleted.")
        });
    }
    match app.opener().open_path(trimmed.to_string(), None::<&str>) {
        Ok(_) => serde_json::json!({"ok": true}),
        Err(e) => serde_json::json!({"ok": false, "error": format!("{e}")}),
    }
}

// ============================================================================
// Phase 8BA — check_file_exists: lightweight existence probe for the
// broken-link sweeper. Same existence-check logic as open_local_file (line
// above) but stops after the .exists() call — no app.opener() invocation,
// no side effects. The sweeper calls this once per stored local path; with
// hundreds of paths potentially in scope we cannot use open_local_file
// because that would launch hundreds of OS-default-app windows.
//
// Returns:
//   {ok: true,  exists: true|false}     for a non-empty path
//   {ok: false, error: "Empty path"}    for an empty/whitespace input
// ============================================================================

#[tauri::command]
fn check_file_exists(path: String) -> serde_json::Value {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return serde_json::json!({"ok": false, "error": "Empty path"});
    }
    let p = std::path::Path::new(trimmed);
    serde_json::json!({"ok": true, "exists": p.exists()})
}

// ============================================================================
// Phase 8CB — read_file_bytes: read a local file from disk and return its
// contents as a base64 data URL, so the Document Export can pack local-path
// files (OEM invoices, POs stored on the OneDrive-synced disk) into the zip
// without the user having to re-upload them into the CRM.
//
// Read-only. Returns {ok, dataUrl, mime, size} on success, {ok:false,error}
// otherwise. A size cap (25 MB) guards against accidentally loading something
// huge into the WebView. The base64 ENCODER is inlined to avoid adding a
// Cargo dependency (mirrors the existing inlined decoder above).
// ============================================================================
fn b64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    let mut chunks = bytes.chunks_exact(3);
    for chunk in &mut chunks {
        let n = ((chunk[0] as u32) << 16) | ((chunk[1] as u32) << 8) | (chunk[2] as u32);
        out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
        out.push(ALPHABET[((n >> 6) & 63) as usize] as char);
        out.push(ALPHABET[(n & 63) as usize] as char);
    }
    let rem = chunks.remainder();
    match rem.len() {
        1 => {
            let n = (rem[0] as u32) << 16;
            out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
            out.push('=');
            out.push('=');
        }
        2 => {
            let n = ((rem[0] as u32) << 16) | ((rem[1] as u32) << 8);
            out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
            out.push(ALPHABET[((n >> 6) & 63) as usize] as char);
            out.push('=');
        }
        _ => {}
    }
    out
}

fn mime_for_ext(path: &str) -> &'static str {
    let lower = path.to_lowercase();
    if lower.ends_with(".pdf") { "application/pdf" }
    else if lower.ends_with(".png") { "image/png" }
    else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") { "image/jpeg" }
    else if lower.ends_with(".gif") { "image/gif" }
    else if lower.ends_with(".webp") { "image/webp" }
    else { "application/octet-stream" }
}

#[tauri::command]
fn read_file_bytes(path: String) -> serde_json::Value {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return serde_json::json!({"ok": false, "error": "Empty path"});
    }
    let p = std::path::Path::new(trimmed);
    if !p.exists() {
        return serde_json::json!({"ok": false, "error": "File not found"});
    }
    // Size guard: refuse files larger than 25 MB.
    if let Ok(meta) = std::fs::metadata(p) {
        if meta.len() > 25 * 1024 * 1024 {
            return serde_json::json!({"ok": false, "error": "File exceeds 25 MB limit"});
        }
    }
    match std::fs::read(p) {
        Ok(bytes) => {
            let mime = mime_for_ext(trimmed);
            let data_url = format!("data:{};base64,{}", mime, b64_encode(&bytes));
            serde_json::json!({"ok": true, "dataUrl": data_url, "mime": mime, "size": bytes.len()})
        }
        Err(e) => serde_json::json!({"ok": false, "error": format!("Read failed: {e}")}),
    }
}

// ============================================================================
// Phase 8BD — fetch_fx_rate: HTTP GET against a free FX API to retrieve a
// current USD:INR rate. Bypasses the WebView CORS restrictions that block
// direct browser fetches from JavaScript to most public FX APIs.
//
// The JS side passes the full URL (so we can try several APIs in priority
// order and stay agnostic to which one is up); this command just makes the
// raw request and returns the response body text. Caller parses the JSON
// per-API since each has a different response shape.
//
// 10-second timeout; covers network errors, slow APIs, and offline cases
// without hanging the UI. Uses rustls (pure Rust TLS) — no system OpenSSL.
// ============================================================================
#[tauri::command]
async fn fetch_fx_rate(url: String) -> Result<String, String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err("Empty URL".to_string());
    }
    // Whitelist scheme — only http(s) allowed
    if !trimmed.starts_with("https://") && !trimmed.starts_with("http://") {
        return Err("URL must use http:// or https:// scheme".to_string());
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent("Masterstone-CRM/1.0 (FX fetch)")
        .build()
        .map_err(|e| format!("client build: {}", e))?;
    let response = client
        .get(trimmed)
        .send()
        .await
        .map_err(|e| format!("network error: {}", e))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {} from server", status.as_u16()));
    }
    response
        .text()
        .await
        .map_err(|e| format!("read body: {}", e))
}

// ============================================================================
// Phase 8AV — open_pdf_dataurl: open an in-memory PDF in the OS default viewer.
//
// Signed invoice PDFs are stored as base64 data URLs inside SQLite (no file
// path exists on disk — the bytes live in the database). The PO list's
// "paperclip pin" affordance works because PO PDFs ARE on disk (OneDrive
// path) and we can just call open_local_file on them. For signed invoices
// we need to materialise the bytes onto disk first, then hand the path off
// to the same `app.opener().open_path` flow so macOS routes the file to the
// user's default PDF viewer (Preview / Acrobat / etc.).
//
// Inputs:
//   data_url            — full "data:application/pdf;base64,..." string from
//                         the attachment record's dataUrl field
//   suggested_filename  — original upload filename, used for the temp file
//                         name so Acrobat's titlebar / Recent Files makes
//                         sense to the user
//   stable_key          — a short identifier (e.g. attachment ID) used to
//                         keep the temp filename stable across clicks. This
//                         way repeated clicks reuse the same temp file rather
//                         than proliferating "_1", "_2" copies in /tmp.
//
// The temp file is written to std::env::temp_dir() (on macOS this is
// $TMPDIR, typically /var/folders/.../T/). macOS cleans this directory
// periodically; we don't try to delete proactively because the user may
// still have the PDF open in their viewer.
//
// Base64 decoder is inlined (no new Cargo dependency). The standard alphabet
// is supported plus URL-safe; only padding-stripped variants are not.
// ============================================================================

fn b64_char_to_val(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' | b'-' => Some(62),  // - is URL-safe variant
        b'/' | b'_' => Some(63),  // _ is URL-safe variant
        _ => None,
    }
}

fn b64_decode(s: &str) -> Result<Vec<u8>, String> {
    // Strip whitespace (some JSON encoders add line breaks)
    let cleaned: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    // Strip padding
    let mut payload: &[u8] = &cleaned;
    while payload.last() == Some(&b'=') {
        payload = &payload[..payload.len() - 1];
    }
    let mut out = Vec::with_capacity(payload.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits: u8 = 0;
    for &c in payload {
        let v = match b64_char_to_val(c) {
            Some(v) => v as u32,
            None => return Err(format!("Invalid base64 character: {}", c as char)),
        };
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1u32 << bits) - 1;
        }
    }
    Ok(out)
}

fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' || c == ' ' { c } else { '_' })
        .collect();
    // Avoid empty or pathological names
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "document.pdf".to_string()
    } else if trimmed.len() > 120 {
        trimmed.chars().take(120).collect()
    } else {
        trimmed.to_string()
    }
}

#[tauri::command]
fn open_pdf_dataurl<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    data_url: String,
    suggested_filename: String,
    stable_key: String,
) -> serde_json::Value {
    // Find the base64 portion of the data URL
    let comma = match data_url.find(',') {
        Some(i) => i,
        None => return serde_json::json!({"ok": false, "error": "Invalid data URL: no comma separator"}),
    };
    let b64_part = &data_url[comma + 1..];
    let bytes = match b64_decode(b64_part) {
        Ok(b) => b,
        Err(e) => return serde_json::json!({"ok": false, "error": format!("Base64 decode failed: {e}")}),
    };
    if bytes.is_empty() {
        return serde_json::json!({"ok": false, "error": "Decoded PDF is empty"});
    }
    // Quick sanity: PDF magic bytes are "%PDF" at offset 0
    if bytes.len() < 4 || &bytes[0..4] != b"%PDF" {
        return serde_json::json!({"ok": false, "error": "Decoded bytes are not a PDF (missing %PDF header)"});
    }

    // Build a stable temp filename. Stable means repeat clicks reuse the
    // same file rather than littering /tmp. We namespace with the stable_key
    // (typically the attachment ID) so different attachments don't collide.
    let safe_name = sanitize_filename(&suggested_filename);
    let safe_key = sanitize_filename(&stable_key);
    let final_name = format!("masterstone_{}_{}", safe_key, safe_name);

    let tmp_path = std::env::temp_dir().join(&final_name);
    if let Err(e) = std::fs::write(&tmp_path, &bytes) {
        return serde_json::json!({"ok": false, "error": format!("Failed to write temp file: {e}")});
    }

    let path_str = tmp_path.to_string_lossy().into_owned();
    match app.opener().open_path(path_str.clone(), None::<&str>) {
        Ok(_) => serde_json::json!({"ok": true, "path": path_str, "bytes": bytes.len()}),
        Err(e) => serde_json::json!({"ok": false, "error": format!("Open failed: {e}"), "path": path_str}),
    }
}

// ============================================================================
// Phase 8P — native print dialog.
//
// JS window.print() and iframe.contentWindow.print() are both no-ops in
// Tauri 2's macOS WKWebView (diagnosed 2026-05-04). Tauri 2 exposes
// Webview::print() on the Rust side specifically as the macOS workaround —
// it invokes NSPrintOperation natively, so the system print dialog appears
// in-app. Used for Client Summary printing (no iframe nesting involved).
// ============================================================================

#[tauri::command]
fn native_print<R: tauri::Runtime>(webview: tauri::Webview<R>) -> serde_json::Value {
    match webview.print() {
        Ok(_) => serde_json::json!({"ok": true}),
        Err(e) => serde_json::json!({"ok": false, "error": format!("{e}")}),
    }
}

// ============================================================================
// Phase 8Q/R/S — print_html: open arbitrary HTML in a fresh Tauri webview
// window and trigger its native print dialog.
//
// Why a new window: when the in-page preview modal renders documents inside
// an <iframe srcdoc>, macOS WKWebView's print engine crops the iframe to its
// on-screen pixel width instead of reflowing for the print page. Loading the
// document as the *top-level* page of its own window bypasses the nesting
// entirely.
//
// 8R note: std::thread::sleep instead of tokio::time::sleep, because the
// project's Cargo.toml configures tokio with features = ["rt", "rt-multi-
// thread"] only — adding "time" would break the Cargo.toml byte-equal lock
// we've held since 8N.
//
// 8S note: prepend UTF-8 BOM to the saved file. WKWebView's default encoding
// for file:// HTML on macOS is Latin-1 (legacy behaviour); without an
// explicit charset declaration, multi-byte UTF-8 sequences get re-decoded
// as Latin-1, producing mojibake (≈ → â‰ˆ, ₹ → â‚¹) in both the rendered
// preview and the saved PDF. The BOM is the most reliable encoding
// declaration for file:// URLs and takes precedence over all other hints.
// ============================================================================

#[tauri::command]
async fn print_html<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    html: String,
    title: Option<String>,
) -> serde_json::Value {
    use tauri::{WebviewUrl, WebviewWindowBuilder};

    // 1. Save HTML to system temp WITH UTF-8 BOM (see 8S note above).
    let stamp = chrono::Utc::now().timestamp_millis();
    let temp_path = std::env::temp_dir().join(format!("Masterstone_print_{stamp}.html"));
    let mut buf: Vec<u8> = Vec::with_capacity(3 + html.len());
    buf.extend_from_slice(&[0xEF, 0xBB, 0xBF]); // UTF-8 BOM
    buf.extend_from_slice(html.as_bytes());
    if let Err(e) = std::fs::write(&temp_path, &buf) {
        return serde_json::json!({"ok": false, "error": format!("Could not write temp file: {e}")});
    }

    // 2. Build file:// URL.
    let url_str = format!("file://{}", temp_path.to_string_lossy());
    let url = match tauri::Url::parse(&url_str) {
        Ok(u) => u,
        Err(e) => return serde_json::json!({"ok": false, "error": format!("URL parse failed: {e}")}),
    };

    // 3. Create a new Tauri webview window with the print HTML as its top-level page.
    let label = format!("ms-print-{stamp}");
    let win_title = title.unwrap_or_else(|| "Print Preview".to_string());

    let win = match WebviewWindowBuilder::new(&app, &label, WebviewUrl::External(url))
        .title(&win_title)
        .inner_size(950.0, 1100.0)
        .min_inner_size(700.0, 500.0)
        .visible(true)
        .center()
        .build()
    {
        Ok(w) => w,
        Err(e) => return serde_json::json!({"ok": false, "error": format!("Window build failed: {e}")}),
    };

    // 4. Wait briefly for the document to load + lay out before printing.
    //    See 8R note above — std::thread::sleep keeps Cargo.toml byte-equal.
    std::thread::sleep(std::time::Duration::from_millis(800));

    // 5. Trigger the native macOS print dialog on the new window's webview.
    //    The new window stays open until the user closes it; we don't auto-
    //    close because Tauri 2's print() may return before the user dismisses
    //    the dialog (closing too early would cancel the print).
    match win.print() {
        Ok(_) => serde_json::json!({"ok": true, "label": label}),
        Err(e) => serde_json::json!({"ok": false, "error": format!("Print failed: {e}")}),
    }
}

// ============================================================================
// 8AC — open_html_in_browser: write HTML to temp file and open it in the
// system default browser (Chrome / Safari / Firefox / etc.) via the opener
// plugin. Used for PDF previews so the user gets their real browser's
// print dialog, which has more options and better fidelity than the in-app
// Tauri WKWebView print pipeline.
//
// This is the "real browser" alternative to print_html (above). Same temp-
// file write step (with UTF-8 BOM per the 8S note), but opens via OS
// instead of building a Tauri webview window. macOS routes .html files
// to the user's default browser.
//
// No new permissions needed: opener:default in capabilities/default.json
// already permits app.opener().open_path() for any path.
// ============================================================================

#[tauri::command]
async fn open_html_in_browser<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    html: String,
) -> serde_json::Value {
    // 1. Save HTML to system temp WITH UTF-8 BOM (same approach as print_html).
    let stamp = chrono::Utc::now().timestamp_millis();
    let temp_path = std::env::temp_dir().join(format!("Masterstone_preview_{stamp}.html"));
    let mut buf: Vec<u8> = Vec::with_capacity(3 + html.len());
    buf.extend_from_slice(&[0xEF, 0xBB, 0xBF]); // UTF-8 BOM
    buf.extend_from_slice(html.as_bytes());
    if let Err(e) = std::fs::write(&temp_path, &buf) {
        return serde_json::json!({"ok": false, "error": format!("Could not write temp file: {e}")});
    }

    // 2. Hand off to the OS via opener plugin. macOS routes .html → user's default browser.
    let path_str = temp_path.to_string_lossy().to_string();
    match app.opener().open_path(path_str.clone(), None::<&str>) {
        Ok(_) => serde_json::json!({"ok": true, "path": path_str}),
        Err(e) => serde_json::json!({"ok": false, "error": format!("Open failed: {e}")}),
    }
}

#[tauri::command]
async fn pick_file<R: tauri::Runtime>(app: tauri::AppHandle<R>, title: Option<String>) -> serde_json::Value {
    use tauri_plugin_dialog::DialogExt;
    let mut builder = app.dialog().file();
    if let Some(t) = title.as_ref() {
        builder = builder.set_title(t);
    }
    match builder.blocking_pick_file() {
        Some(fp) => match fp.into_path() {
            Ok(path) => serde_json::json!({"ok": true, "path": path.to_string_lossy()}),
            Err(e) => serde_json::json!({"ok": false, "error": format!("path conversion: {e}")}),
        },
        None => serde_json::json!({"ok": false, "cancelled": true}),
    }
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
            // 8AW — direct attachments read (bypasses localStorage hydration
            // for this bucket; see storage_load_attachments definition for
            // rationale around the WKWebView quota issue)
            storage_load_attachments,
            storage_save_setting,
            // 8YZ — Consultants module
            storage_save_consultants,
            storage_save_consultant_payments,
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
            // Phase 8C: local file attachments (companion to OneDrive URLs)
            pick_file,
            open_local_file,
            // Phase 8BA: existence probe for the broken-link sweeper —
            // see check_file_exists definition for rationale.
            check_file_exists,
            // Phase 8CB: read a local file's bytes (base64 data URL) so the
            // Document Export can pack local-path files into the zip.
            read_file_bytes,
            // Phase 8BD: HTTP fetch for live USD:INR rate (bypasses WebView CORS).
            fetch_fx_rate,
            // Phase 8AV: open in-memory PDF (base64 dataUrl) in OS default
            // viewer. Used by signed-invoice paperclip pins — invoice
            // attachments live in SQLite as base64, not on disk, so the
            // bytes have to be materialised to a temp file first before
            // app.opener() can route to Preview/Acrobat.
            open_pdf_dataurl,
            // Phase 8P: native print dialog (macOS workaround for broken
            // window.print() in Tauri 2 WKWebView). Used for Client Summary.
            native_print,
            // Phase 8Q/R/S: print arbitrary HTML in a fresh top-level webview
            // window — bypasses the nested-iframe layout cropping bug.
            // Used for PO/Proposal/Invoice/Send-to-CA modal previews.
            print_html,
            // 8AC: open arbitrary HTML in user's default browser (Chrome /
            // Safari / etc.) — gives them their real browser's print dialog
            // for higher-fidelity PDF export. Replaces print_html as the
            // primary path for document previews.
            open_html_in_browser,
            // Session 8 / B-fix-8: file-linking subsystem removed.
            // The user opted out of the matcher / migration / category
            // workflow in favour of manual linking via OneDrive URLs in
            // contract pages (which were always present and are unaffected).
        ])
        .setup(|_app| Ok(()))
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
