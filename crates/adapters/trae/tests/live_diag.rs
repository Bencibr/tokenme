//! `#[ignore]`d live-store diagnostic: decrypt the machine's real Trae stores
//! and report what the turn query would see. Run with
//! `cargo test --test live_diag -- --ignored --nocapture`.

use usage_adapter_trae::crypto;

fn diag(edition: &str) {
    let Some(db) = dirs::data_dir().map(|d| d.join(edition).join("ModularData").join("ai-agent").join("database.db"))
    else {
        return;
    };
    println!("== {edition}: {}", db.display());
    if !db.is_file() {
        println!("   absent");
        return;
    }
    let wal = db.with_file_name("database.db-wal");
    let image = match crypto::decrypt_report(&db, wal.is_file().then(|| wal.as_path())) {
        Ok(image) => {
            println!("   decrypt: ok, {} bytes", image.len());
            image
        }
        Err(reason) => {
            println!("   decrypt FAILED: {reason}");
            return;
        }
    };
    let (conn, temp) = match crypto::open_image_checked(&image) {
        Ok(v) => v,
        Err(reason) => {
            println!("   open FAILED: {reason}");
            return;
        }
    };
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table'")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    println!("   tables: {tables:?}");
    for sql in [
        "SELECT COUNT(*) FROM chat_turn",
        "SELECT COUNT(*) FROM chat_turn WHERE deleted_at = 0",
        "SELECT COUNT(*) FROM chat_turn WHERE deleted_at = 0 AND turn_status = 'completed'",
    ] {
        match conn.query_row(sql, [], |r| r.get::<_, i64>(0)) {
            Ok(n) => println!("   {sql} -> {n}"),
            Err(e) => println!("   {sql} -> ERR {e}"),
        }
    }
    let _ = std::fs::remove_file(temp);
}

#[test]
#[ignore]
fn live_stores_diag() {
    diag("Trae");
    diag("Trae CN");
}
