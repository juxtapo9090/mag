// Run with: MAG_URL=unix:///run/mag/zet.sock cargo run --example unix_smoke
// and:      MAG_URL=http://127.0.0.1:7734 cargo run --example unix_smoke
// Posts a real /toon command so the audit log records a seat attribution.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let raw = std::env::var("MAG_URL").unwrap_or_else(|_| "http://127.0.0.1:7734".into());
    let mut b = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(15));
    let (socket, base) =
        if let Some(path) = raw.strip_prefix("unix://").map(|r| r.trim_end_matches('/')) {
            b = b.unix_socket(path);
            (Some(path.to_string()), "http://localhost".to_string())
        } else {
            (None, raw.trim_end_matches('/').to_string())
        };
    let client = b.build()?;

    // GET /health
    let h = client.get(format!("{}/health", base)).send()?;
    println!("MAG_URL={} socket={:?} health={}", raw, socket, h.status());

    // POST /toon — a trivial command so the audit row lands with a seat.
    let body = r#"{"cmd":"echo phase3-socket-transport-ok"}"#;
    let resp = client
        .post(format!("{}/toon", base))
        .header("content-type", "application/json")
        .body(body)
        .send()?;
    println!("toon status={} body={}", resp.status(), resp.text()?);
    Ok(())
}
