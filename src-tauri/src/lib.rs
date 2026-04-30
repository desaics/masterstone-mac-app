// Masterstone CRM — Tauri runtime entry.
//
// Session 3 scope: a single command, `storage_load_all`, that opens the
// SQLite database, reads every table, and reconstructs the same shape the
// HTML CRM expects in localStorage. The frontend bootstrap shim (dist/index.html)
// invokes this once at startup, populates localStorage from the result, and
// then navigates to dist/crm.html — at which point the existing CRM init runs
// against fully-populated localStorage, none the wiser.
//
// Read-only at this stage: the command produces a snapshot, no writes go back.
// Session 4 will add per-key write commands and wire the storage adapter.

use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Where the SQLite file is expected to live on macOS.
/// Path: ~/Library/Application Support/com.masterstone.crm/masterstone.db
fn db_path() -> Option<PathBuf> {
    let mut p = dirs::data_dir()?;
    p.push("com.masterstone.crm");
    p.push("masterstone.db");
    Some(p)
}

/// Make sure the parent directory exists so a user navigating in Finder
/// can drop the .db file in even on first launch (Tauri itself doesn't
/// create this folder for us until app data APIs are used).
fn ensure_parent_exists(path: &PathBuf) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
}

/// Result envelope returned to the frontend.
/// `data` is a map of localStorage keys → JSON-encoded string values
/// (matching how localStorage really stores things in the browser).
/// On error, `error_kind` and `error_detail` are populated and `data` is empty.
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

/// The actual command. Wrapped so that any panic or rusqlite error becomes
/// a structured error in the result rather than a hard failure across the IPC boundary.
#[tauri::command]
fn storage_load_all() -> LoadAllResult {
    let path = match db_path() {
        Some(p) => p,
        None => {
            return LoadAllResult::err(
                "PATH_RESOLVE_FAILED",
                "Could not resolve macOS Application Support directory.".to_string(),
                None,
            );
        }
    };

    if !path.exists() {
        ensure_parent_exists(&path);
        return LoadAllResult::err(
            "DB_NOT_FOUND",
            format!(
                "Database file not found at expected location. \
                 Place masterstone.db at: {}",
                path.display()
            ),
            Some(path),
        );
    }

    match load_all_inner(&path) {
        Ok(result) => result,
        Err(e) => LoadAllResult::err(
            "DB_READ_ERROR",
            format!("{e}"),
            Some(path),
        ),
    }
}

/// Inner function — does the actual SQLite work, returns Result so callers can
/// uniformly convert errors to the structured envelope.
fn load_all_inner(path: &PathBuf) -> Result<LoadAllResult, rusqlite::Error> {
    let con = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;

    let mut data: BTreeMap<String, String> = BTreeMap::new();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();

    // -----------------------------------------------------------------------
    // ms_pro_v210 — array of contracts. Reconstruct from the `raw_data` JSON
    // column, ordered by `legacy_idx` to preserve original array order
    // (invoices reference contracts by `linkedContractIdx` = array position).
    // -----------------------------------------------------------------------
    let contracts = collect_array(&con,
        "SELECT raw_data FROM contracts ORDER BY legacy_idx ASC")?;
    counts.insert("contracts".into(), contracts.len());
    data.insert("ms_pro_v210".into(), serde_json::Value::Array(contracts).to_string());

    // -----------------------------------------------------------------------
    // ms_client_master_v1 — name-keyed dict. Reconstruct from raw_data,
    // re-keying by the company_name column.
    // -----------------------------------------------------------------------
    let clients_dict = collect_dict_by_column(&con,
        "SELECT company_name, raw_data FROM clients")?;
    counts.insert("clients".into(), clients_dict.len());
    data.insert("ms_client_master_v1".into(),
        serde_json::Value::Object(clients_dict).to_string());

    // -----------------------------------------------------------------------
    // ms_reseller_master_v1 — name-keyed dict.
    // -----------------------------------------------------------------------
    let resellers_dict = collect_dict_by_column(&con,
        "SELECT company_name, raw_data FROM resellers")?;
    counts.insert("resellers".into(), resellers_dict.len());
    data.insert("ms_reseller_master_v1".into(),
        serde_json::Value::Object(resellers_dict).to_string());

    // -----------------------------------------------------------------------
    // ms_oem_master_v1 — name → product list dict. Built from products table
    // (since the per-OEM raw_data only has products, not actual OEM metadata).
    // -----------------------------------------------------------------------
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
    data.insert("ms_oem_master_v1".into(),
        serde_json::Value::Object(oems_obj).to_string());

    // -----------------------------------------------------------------------
    // ms_invoices_v1 — array.
    // -----------------------------------------------------------------------
    let invoices = collect_array(&con,
        "SELECT raw_data FROM invoices ORDER BY created_at, id")?;
    counts.insert("invoices".into(), invoices.len());
    data.insert("ms_invoices_v1".into(),
        serde_json::Value::Array(invoices).to_string());

    // -----------------------------------------------------------------------
    // ms_prospects_v1 — array.
    // -----------------------------------------------------------------------
    let prospects = collect_array(&con,
        "SELECT raw_data FROM prospects ORDER BY created_at, id")?;
    counts.insert("prospects".into(), prospects.len());
    data.insert("ms_prospects_v1".into(),
        serde_json::Value::Array(prospects).to_string());

    // -----------------------------------------------------------------------
    // ms_proposals_v1 — array (currently empty).
    // -----------------------------------------------------------------------
    let proposals = collect_array(&con,
        "SELECT raw_data FROM proposals ORDER BY id")?;
    counts.insert("proposals".into(), proposals.len());
    data.insert("ms_proposals_v1".into(),
        serde_json::Value::Array(proposals).to_string());

    // -----------------------------------------------------------------------
    // ms_purchase_orders_v1 — array (currently empty).
    // -----------------------------------------------------------------------
    let pos = collect_array(&con,
        "SELECT raw_data FROM purchase_orders ORDER BY id")?;
    counts.insert("purchase_orders".into(), pos.len());
    data.insert("ms_purchase_orders_v1".into(),
        serde_json::Value::Array(pos).to_string());

    // -----------------------------------------------------------------------
    // ms_commissions_v1 — { accruals: [...], payouts: [...] }.
    // -----------------------------------------------------------------------
    let accruals = collect_array(&con,
        "SELECT raw_data FROM commission_accruals ORDER BY accrual_date, id")?;
    let payouts = collect_array(&con,
        "SELECT raw_data FROM commission_payouts ORDER BY payout_date, id")?;
    counts.insert("commission_accruals".into(), accruals.len());
    counts.insert("commission_payouts".into(), payouts.len());
    let mut commissions_obj = serde_json::Map::new();
    commissions_obj.insert("accruals".into(), serde_json::Value::Array(accruals));
    commissions_obj.insert("payouts".into(), serde_json::Value::Array(payouts));
    data.insert("ms_commissions_v1".into(),
        serde_json::Value::Object(commissions_obj).to_string());

    // -----------------------------------------------------------------------
    // ms_company_profile_v1 — single dict.
    // -----------------------------------------------------------------------
    {
        let mut stmt = con.prepare(
            "SELECT raw_data FROM company_profile WHERE id = 1")?;
        let mut rows = stmt.query([])?;
        if let Some(row) = rows.next()? {
            let raw: String = row.get(0)?;
            data.insert("ms_company_profile_v1".into(), raw);
            counts.insert("company_profile".into(), 1);
        } else {
            // Empty profile fallback so the CRM doesn't crash on null
            data.insert("ms_company_profile_v1".into(), "{}".to_string());
            counts.insert("company_profile".into(), 0);
        }
    }

    // -----------------------------------------------------------------------
    // ms_attachments_v1 — array (currently empty).
    // -----------------------------------------------------------------------
    let attachments = collect_array(&con,
        "SELECT raw_data FROM attachments ORDER BY uploaded_at, id")?;
    counts.insert("attachments".into(), attachments.len());
    data.insert("ms_attachments_v1".into(),
        serde_json::Value::Array(attachments).to_string());

    // -----------------------------------------------------------------------
    // app_settings — flat key-value rows. Most map to a stand-alone localStorage
    // key (theme → ms_theme, fy_basis → ms_fy_basis_v1, etc.). The two JSON-blob
    // settings (schema_meta, migration_4A_state) get unwrapped back to their
    // localStorage keys.
    // -----------------------------------------------------------------------
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
                _ => continue, // unknown setting — skip
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

/// Helper: run a single-column query returning JSON strings, parse each row
/// as JSON, return as a Vec of Values for embedding into a larger structure.
fn collect_array(con: &Connection, sql: &str) -> Result<Vec<serde_json::Value>, rusqlite::Error> {
    let mut stmt = con.prepare(sql)?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut out: Vec<serde_json::Value> = Vec::new();
    for row in rows {
        let raw = row?;
        match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(v) => out.push(v),
            Err(_) => out.push(serde_json::Value::Null),  // tolerate bad row, don't fail entire load
        }
    }
    Ok(out)
}

/// Helper: run a two-column (key, raw_data) query and return as a JSON object.
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
        let v = serde_json::from_str::<serde_json::Value>(&raw)
            .unwrap_or(serde_json::Value::Null);
        out.insert(k, v);
    }
    Ok(out)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![storage_load_all])
        .setup(|_app| Ok(()))
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
