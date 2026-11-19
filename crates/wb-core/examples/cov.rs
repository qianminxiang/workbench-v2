// 存量公告实测工具：项目编号覆盖率 + 阶段判据（A 路线）命中率。
//
// 用法：cargo run -p wb-core --example cov -- <raw.json> [--verbose]
//
// 判据全部取自交付代码本体：
//   - 编号：`wb_core::lead_dedup::extract_project_code`（v3.1.0 起 L3 归一同一份实现）
//   - 阶段：`wb_core::intel_stage::stage_from_text`（本版新增的 A 路线）
// 不在脚本里复述任何规则，避免「测的是脚本自己的假设」。

use std::collections::{BTreeMap, BTreeSet};

use wb_core::intel_stage::stage_from_text;
use wb_core::lead_dedup::extract_project_code;

fn main() {
    let mut args = std::env::args().skip(1);
    let p = args.next().expect("need raw.json path");
    let verbose = args.any(|a| a == "--verbose");
    let s = std::fs::read_to_string(&p).expect("read raw json");
    let v: serde_json::Value = serde_json::from_str(&s).expect("parse raw json");
    let recs = v["result"]["records"].as_array().expect("records array");

    // totalcount 在 Epoint 同族接口里可能是数字，也可能被包成字符串，两种都收。
    let tv = &v["result"]["totalcount"];
    let total: i64 = tv
        .as_i64()
        .or_else(|| tv.as_str().and_then(|s| s.trim().parse::<i64>().ok()))
        .unwrap_or(0);

    // 与 lead_dedup::normalize_project_code 同口径的极简版，仅用于「正文抽取 vs 接口字段」比对。
    fn norm_field(raw: &str) -> String {
        let mut out = String::new();
        for ch in raw.chars() {
            let o = ch as u32;
            let c = if (0xFF01..=0xFF5E).contains(&o) {
                char::from_u32(o - 0xFEE0).unwrap_or(ch)
            } else {
                ch
            };
            if c.is_whitespace() || "-_./\\".contains(c) || "【〔[]〕]】".contains(c) {
                continue;
            }
            out.push(c.to_ascii_lowercase());
        }
        out
    }

    let mut with_code = 0usize;
    let mut agree_field = 0usize;
    let mut field_present = 0usize;
    let mut stage_hit = 0usize;
    let mut stage_by_label: BTreeMap<String, usize> = BTreeMap::new();
    let mut code_set: BTreeSet<String> = BTreeSet::new();
    let mut field_set: BTreeSet<String> = BTreeSet::new();
    let mut dates: Vec<String> = Vec::new();
    let mut rows: Vec<serde_json::Value> = Vec::new();

    for r in recs {
        let t = r["title"].as_str().unwrap_or("");
        let c = r["content"].as_str().unwrap_or("");
        let text = format!("{}\n{}", t, c);

        let got = extract_project_code(&text);
        if let Some(code) = &got {
            with_code += 1;
            code_set.insert(code.clone());
        }

        let field = r["projectno"].as_str().unwrap_or("").trim().to_string();
        let field_norm = if field.is_empty() { String::new() } else { norm_field(&field) };
        if !field_norm.is_empty() {
            field_present += 1;
            field_set.insert(field_norm.clone());
            if got.as_ref().map(|g| g == &field_norm).unwrap_or(false) {
                agree_field += 1;
            }
        }

        // 阶段判据 A：标题优先，正文取前 300 字（与 intel_store 落库时的入参同形）
        let head: String = c.chars().take(300).collect();
        let stage = stage_from_text(t, &head);
        match &stage {
            Some((st, _w)) => {
                stage_hit += 1;
                *stage_by_label.entry(st.label().to_string()).or_insert(0) += 1;
            }
            None => {
                *stage_by_label.entry("阶段未知".to_string()).or_insert(0) += 1;
            }
        }

        if let Some(d) = r["webdate"].as_str() {
            if !d.is_empty() {
                dates.push(d.to_string());
            }
        }

        if verbose {
            rows.push(serde_json::json!({
                "title": t.chars().take(60).collect::<String>(),
                "webdate": r["webdate"].as_str().unwrap_or(""),
                "projectno_field": field,
                "extracted": got.clone().unwrap_or_default(),
                "agree_with_field": if field_norm.is_empty() { serde_json::Value::Null } else { serde_json::json!(got.as_ref().map(|g| g == &field_norm).unwrap_or(false)) },
                "stage_rule": stage.as_ref().map(|(s, _)| s.label().to_string()).unwrap_or_else(|| "阶段未知".to_string()),
                "stage_word": stage.as_ref().map(|(_, w)| w.to_string()).unwrap_or_default(),
            }));
        }
    }

    dates.sort();
    let n = recs.len();
    let mut out = serde_json::json!({
        "totalcount_declared": total,
        "records_loaded": n,
        "date_min": dates.first().cloned().unwrap_or_default(),
        "date_max": dates.last().cloned().unwrap_or_default(),
        "with_code": with_code,
        "ratio_pct": if n == 0 { 0.0 } else { with_code as f64 / n as f64 * 100.0 },
        "distinct_extracted_codes": code_set.len(),
        "projectno_field_present": field_present,
        "distinct_field_codes": field_set.len(),
        "agree_with_field": agree_field,
        "stage_rule_hit": stage_hit,
        "stage_rule_hit_pct": if n == 0 { 0.0 } else { stage_hit as f64 / n as f64 * 100.0 },
        "stage_by_label": stage_by_label,
    });
    if verbose {
        out["rows"] = serde_json::Value::Array(rows);
    }
    println!("{}", out);
}
