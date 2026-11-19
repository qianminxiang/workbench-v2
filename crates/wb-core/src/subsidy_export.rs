//! 补贴明细一键导出（v2.3.0 需求⑤）。
//!
//! 语义：**按当前筛选条件全量导出**，不是只导当前页——这是本模块存在的理由。
//! UI 侧（补贴明细视图工具栏「导出」按钮）把视图当前筛选序列化成 `Vec<ExportFilter>`
//! 传进来，引擎对**整个数据集**应用筛选后写出，分页只是显示层概念。
//!
//! 格式：xlsx 为主（手写 OOXML，zip 4.6 打包——calamine 只读不能写，且不为一个
//! 导出按钮引入 xlsxwriter 全家桶）；留 CSV 口子（`export_csv`，BOM + UTF-8，
//! Excel 直开不乱码）。

use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::Path;

/// 单条筛选条件。多条之间是 AND。
#[derive(Debug, Clone, PartialEq)]
pub struct ExportFilter {
    /// 字段名（表头，必须精确匹配，找不到直接报错——静默跳过等于「筛选没生效却导出了全量」）
    pub field: String,
    pub op: FilterOp,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FilterOp {
    /// 相等（精确）
    Eq,
    /// 包含（子串，大小写不敏感）
    Contains,
}

impl ExportFilter {
    pub fn eq(field: &str, value: &str) -> Self {
        Self { field: field.to_string(), op: FilterOp::Eq, value: value.to_string() }
    }
    pub fn contains(field: &str, value: &str) -> Self {
        Self { field: field.to_string(), op: FilterOp::Contains, value: value.to_string() }
    }

    fn matches(&self, headers: &[String], row: &[String]) -> Result<bool, String> {
        let idx = headers
            .iter()
            .position(|h| h == &self.field)
            .ok_or_else(|| format!("筛选字段「{}」不在表头里（表头：{}）", self.field, headers.join("、")))?;
        let cell = row.get(idx).map(|s| s.as_str()).unwrap_or("");
        Ok(match self.op {
            FilterOp::Eq => cell == self.value,
            FilterOp::Contains => cell.to_lowercase().contains(&self.value.to_lowercase()),
        })
    }
}

/// 对全量数据应用筛选（AND 语义）。无筛选 = 原样全量返回。
/// 任一筛选字段在表头中不存在 → Err（不允许静默失效）。
pub fn apply_filters<'a>(
    headers: &[String],
    rows: &'a [Vec<String>],
    filters: &[ExportFilter],
) -> Result<Vec<&'a Vec<String>>, String> {
    let mut out = Vec::with_capacity(rows.len());
    'row: for r in rows {
        for f in filters {
            if !f.matches(headers, r)? {
                continue 'row;
            }
        }
        out.push(r);
    }
    Ok(out)
}

// ───────────────────────────── CSV ─────────────────────────────

/// CSV 导出（UTF-8 + BOM，Excel 双击直开）。返回写出的数据行数（不含表头）。
pub fn export_csv(path: &Path, headers: &[String], rows: &[Vec<String>]) -> Result<usize, String> {
    let width = headers.len();
    let mut buf = String::with_capacity(64 * 1024);
    buf.push('\u{feff}'); // BOM
    buf.push_str(&csv_line(headers, width));
    for r in rows {
        buf.push_str(&csv_line(r, width));
    }
    let mut f = fs::File::create(path).map_err(|e| format!("创建 {} 失败：{e}", path.display()))?;
    f.write_all(buf.as_bytes())
        .map_err(|e| format!("写 {} 失败：{e}", path.display()))?;
    Ok(rows.len())
}

fn csv_line(cells: &[String], width: usize) -> String {
    let mut s = String::new();
    for i in 0..width {
        if i > 0 {
            s.push(',');
        }
        let c = cells.get(i).map(|x| x.as_str()).unwrap_or("");
        // RFC 4180：含逗号/引号/换行的字段加引号，内部引号翻倍
        if c.contains(',') || c.contains('"') || c.contains('\n') || c.contains('\r') {
            let _ = write!(s, "\"{}\"", c.replace('"', "\"\""));
        } else {
            s.push_str(c);
        }
    }
    s.push('\n');
    s
}

// ───────────────────────────── xlsx（手写 OOXML） ─────────────────────────────

/// xlsx 导出（手写 OOXML，zip 4.6 打包）。返回写出的数据行数（不含表头）。
/// 数字语义：去千分位逗号后能解析为数的写数值格（Excel 里可求和排序），
/// 其余（编号/日期串/状态等）写 inline string——「显示与源一致」优先。
pub fn export_xlsx(path: &Path, headers: &[String], rows: &[Vec<String>], sheet_name: &str) -> Result<usize, String> {
    let width = headers.len();
    let total_rows = rows.len() + 1; // +表头

    // sheet1.xml（流式拼进 String，34k 行 × 15 列在 4GB 沙箱可承受；
    // 真到内存吃紧再换 zip 的流式 writer，当前规模实测无需）
    let mut sheet = String::with_capacity(48 * 1024 * 1024.min(rows.len() * 512 + 4096));
    sheet.push_str(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>"#);
    sheet.push_str(&row_xml(1, headers, width, true));
    for (i, r) in rows.iter().enumerate() {
        sheet.push_str(&row_xml(i + 2, r, width, false));
    }
    sheet.push_str("</sheetData></worksheet>");

    let xlsx = build_zip(sheet_name, &sheet, total_rows, width)?;

    let mut f = fs::File::create(path).map_err(|e| format!("创建 {} 失败：{e}", path.display()))?;
    f.write_all(&xlsx)
        .map_err(|e| format!("写 {} 失败：{e}", path.display()))?;
    Ok(rows.len())
}

fn row_xml(row_no: usize, cells: &[String], width: usize, bold: bool) -> String {
    let mut s = format!(r#"<row r="{}">"#, row_no);
    for c in 0..width {
        let val = cells.get(c).map(|x| x.as_str()).unwrap_or("");
        let col = col_name(c);
        let _ = write!(s, r#"<c r="{}{}""#, col, row_no);
        if bold {
            s.push_str(r#" s="1""#); // 表头样式（styles.xml 里定义的加粗）
        }
        // 数字：去千分位逗号后可解析 → 数值格
        let numeric = val.replace(',', "");
        if !numeric.is_empty() && numeric.parse::<f64>().is_ok() && !looks_like_id(val) {
            let _ = write!(s, "><v>{}</v></c>", trim_num(&numeric));
        } else {
            let _ = write!(s, r#" t="inlineStr"><is><t xml:space="preserve">{}</t></is></c>"#, xml_escape(val));
        }
    }
    s.push_str("</row>");
    s
}

/// 出厂编号这类「纯数字标识」不写数值格：一旦写成数字，前导零丢失、
/// 大数变科学计数法，跨系统对账必错。判据：字段叫「编号/序号」语义由调用方
/// 的表头位置决定太脆，这里按「纯数字且长度 ≥ 10」保守处理（正常量值不会 10 位纯整数）。
fn looks_like_id(val: &str) -> bool {
    val.len() >= 10 && val.chars().all(|c| c.is_ascii_digit())
}

fn trim_num(s: &str) -> String {
    // f64 解析成功才进来；整数语义去掉尾部 .0，让 Excel 显示 2024 而不是 2024.0
    if let Ok(i) = s.parse::<i64>() {
        return i.to_string();
    }
    s.to_string()
}

fn col_name(mut idx: usize) -> String {
    let mut s = String::new();
    loop {
        s.insert(0, (b'A' + (idx % 26) as u8) as char);
        idx = idx / 26;
        if idx == 0 {
            break;
        }
        idx -= 1;
    }
    s
}

fn xml_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            // XML 1.0 非法控制字符直接丢弃（源数据是网页抓来的，可能带 \x01 之类脏字节）
            c if (c as u32) < 0x20 && c != '\t' && c != '\n' && c != '\r' => {}
            c => o.push(c),
        }
    }
    o
}

fn build_zip(sheet_name: &str, sheet_xml: &str, rows: usize, cols: usize) -> Result<Vec<u8>, String> {
    use std::io::{Cursor, Write};
    use zip::write::SimpleFileOptions;
    use zip::ZipWriter;

    let mut buf: Vec<u8> = Vec::with_capacity(sheet_xml.len() + 16 * 1024);
    {
        // Cursor 包装：ZipWriter 要求底层 Seek，裸 Vec<u8> 不满足
        let cursor = Cursor::new(&mut buf);
        let mut w = ZipWriter::new(cursor);
        let opts = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);

        let content_types = format!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/></Types>"#
        );
        let rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#;
        let workbook = format!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="{}" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
            xml_escape(sheet_name)
        );
        let wb_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#;
        let styles = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><fonts count="2"><font><sz val="11"/><name val="Calibri"/></font><font><b/><sz val="11"/><name val="Calibri"/></font></fonts><fills count="2"><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="gray125"/></fill></fills><borders count="1"><border/></borders><cellStyleXfs count="1"><xf/></cellStyleXfs><cellXfs count="2"><xf xfId="0"/><xf xfId="0" fontId="1" applyFont="1"/></cellXfs></styleSheet>"#;

        let mut put = |name: &str, data: &str| -> Result<(), String> {
            w.start_file(name, opts).map_err(|e| format!("zip 写 {name} 失败：{e}"))?;
            w.write_all(data.as_bytes()).map_err(|e| format!("zip 写 {name} 失败：{e}"))?;
            Ok(())
        };
        put("[Content_Types].xml", &content_types)?;
        put("_rels/.rels", rels)?;
        put("xl/workbook.xml", &workbook)?;
        put("xl/_rels/workbook.xml.rels", wb_rels)?;
        put("xl/styles.xml", styles)?;
        put("xl/worksheets/sheet1.xml", sheet_xml)?;
        w.finish().map_err(|e| format!("zip 收尾失败：{e}"))?;
    }
    let _ = (rows, cols); // 行列数供将来 dimension 标签用，当前省略（Excel 自适应）
    Ok(buf)
}

// ───────────────────────────── CSV 读取（演示数据装载） ─────────────────────────────

/// 读 CSV（容忍 BOM / CRLF / 引号字段）。供 wbd 演示链路与 UI「从明细表装载」共用。
pub fn read_csv(path: &Path) -> Result<(Vec<String>, Vec<Vec<String>>), String> {
    let raw = fs::read(path).map_err(|e| format!("读 {} 失败：{e}", path.display()))?;
    let text = String::from_utf8_lossy(&raw);
    let text = text.trim_start_matches('\u{feff}');
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut field = String::new();
    let mut row: Vec<String> = Vec::new();
    let mut in_quotes = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    field.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(c);
            }
        } else {
            match c {
                '"' => in_quotes = true,
                ',' => {
                    row.push(std::mem::take(&mut field));
                }
                '\r' => {}
                '\n' => {
                    row.push(std::mem::take(&mut field));
                    if !(row.len() == 1 && row[0].is_empty()) {
                        rows.push(std::mem::take(&mut row));
                    } else {
                        row.clear();
                    }
                }
                c => field.push(c),
            }
        }
    }
    row.push(field);
    if !(row.len() == 1 && row[0].is_empty()) {
        rows.push(row);
    }
    if rows.is_empty() {
        return Err("CSV 是空的".into());
    }
    let headers = rows.remove(0);
    Ok((headers, rows))
}

// ───────────────────────────── 测试 ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn demo() -> (Vec<String>, Vec<Vec<String>>) {
        let headers: Vec<String> = ["年份", "县", "购机者姓名", "购买数量(台)", "总补贴额(元)", "出厂编号", "状态"]
            .iter().map(|s| s.to_string()).collect();
        let rows = vec![
            vec!["2024", "颍东区", "昊博农机专业合作社", "1", "12,000.00", "82173115PR52", "资金已兑付"],
            vec!["2024", "蒙城县", "云农植保服务有限公司", "1", "12,000.00", "821111600822", "资金已兑付"],
            vec!["2023", "颍东区", "张某", "2", "24,000.00", "0123456789012", "已公示"],
        ].into_iter().map(|r| r.into_iter().map(|s| s.to_string()).collect()).collect();
        (headers, rows)
    }

    #[test]
    fn filters_apply_over_full_dataset_not_page() {
        let (h, rows) = demo();
        // 筛选：年份=2024（UI 分页只显示第 1 页 1 条时，导出仍须给全量 2 条）
        let f = [ExportFilter::eq("年份", "2024")];
        let got = apply_filters(&h, &rows, &f).unwrap();
        assert_eq!(got.len(), 2);
        // 无筛选 = 全量
        let all = apply_filters(&h, &rows, &[]).unwrap();
        assert_eq!(all.len(), 3);
        // AND 语义
        let f2 = [ExportFilter::eq("年份", "2024"), ExportFilter::contains("县", "颍东")];
        assert_eq!(apply_filters(&h, &rows, &f2).unwrap().len(), 1);
        // 字段不存在必须报错（不允许静默导全量）
        assert!(apply_filters(&h, &rows, &[ExportFilter::eq("不存在", "x")]).is_err());
    }

    #[test]
    fn xlsx_roundtrip_via_calamine() {
        let (h, rows) = demo();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("out.xlsx");
        let n = export_xlsx(&p, &h, &rows, "补贴明细").unwrap();
        assert_eq!(n, 3);
        // 用 calamine（读侧既有依赖）读回：行数、表头、关键单元格
        use calamine::Reader as _;
        let mut book = calamine::open_workbook_auto(&p).unwrap();
        let range = book.worksheet_range("补贴明细").unwrap();
        assert_eq!(range.height(), 4); // 表头 + 3
        assert_eq!(range.width(), 7);
        assert_eq!(range.get((0, 0)).unwrap().to_string(), "年份");
        // 金额带千分位逗号 → 数值格（读回是 Float/Int）
        match range.get((1, 4)).unwrap() {
            calamine::Data::Float(f) => assert_eq!(*f, 12000.0),
            calamine::Data::Int(i) => assert_eq!(*i, 12000),
            other => panic!("金额应是数值格，得到 {other:?}"),
        }
        // 出厂编号 12 位纯数字 → inline string，前导零/原样保留
        assert_eq!(range.get((3, 5)).unwrap().to_string(), "0123456789012");
        assert_eq!(range.get((1, 5)).unwrap().to_string(), "82173115PR52");
    }

    #[test]
    fn csv_roundtrip_keeps_headers_and_quoting() {
        let (h, rows) = demo();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("out.csv");
        let n = export_csv(&p, &h, &rows).unwrap();
        assert_eq!(n, 3);
        let (h2, rows2) = read_csv(&p).unwrap();
        assert_eq!(h2, h);
        assert_eq!(rows2.len(), 3);
        assert_eq!(rows2[0][4], "12,000.00");
    }
}
