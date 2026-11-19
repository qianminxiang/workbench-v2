use wb_core::lead_dedup::extract_project_code;
fn main() {
    let p = std::env::args().nth(1).unwrap();
    let s = std::fs::read_to_string(&p).unwrap();
    let v: serde_json::Value = serde_json::from_str(&s).unwrap();
    let recs = v["result"]["records"].as_array().unwrap();
    let total: i64 = v["result"]["totalcount"].as_str().unwrap_or("0").parse().unwrap_or(0);
    let mut hit = 0usize;
    let mut samples: Vec<(String, String)> = Vec::new();
    for r in recs {
        let t = r["title"].as_str().unwrap_or("");
        let c = r["content"].as_str().unwrap_or("");
        let text = format!("{}\n{}", t, c);
        match extract_project_code(&text) {
            Some(code) => { hit += 1; if samples.len() < 8 { samples.push((t.chars().take(40).collect(), code)); } }
            None => {}
        }
    }
    println!("{}", serde_json::json!({
        "totalcount_declared": total,
        "records_loaded": recs.len(),
        "with_code": hit,
        "ratio_pct": if recs.is_empty() { 0.0 } else { (hit as f64 / recs.len() as f64 * 100.0) },
        "samples": samples,
    }));
}
