use anyhow::{bail, Result};

/// Parse a 1-based page-range expression ("1-3, 5, 8-") against a page count.
/// Returns sorted, de-duplicated 1-based page numbers.
pub fn parse_ranges(input: &str, page_count: u32) -> Result<Vec<u32>> {
    let mut out = std::collections::BTreeSet::new();
    let parts: Vec<&str> = input.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        bail!("empty page range");
    }
    for part in parts {
        if let Some((a, b)) = part.split_once('-') {
            let start: u32 = if a.trim().is_empty() { 1 } else { a.trim().parse()? };
            let end: u32 = if b.trim().is_empty() { page_count } else { b.trim().parse()? };
            if start < 1 || end > page_count || start > end {
                bail!("range \"{part}\" is out of bounds (pages go from 1 to {page_count})");
            }
            out.extend(start..=end);
        } else {
            let p: u32 = part.parse()?;
            if p < 1 || p > page_count {
                bail!("page {p} is out of bounds (pages go from 1 to {page_count})");
            }
            out.insert(p);
        }
    }
    Ok(out.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mixed() {
        assert_eq!(parse_ranges("1-3, 5", 10).unwrap(), vec![1, 2, 3, 5]);
        assert_eq!(parse_ranges("8-", 10).unwrap(), vec![8, 9, 10]);
        assert_eq!(parse_ranges("-2", 10).unwrap(), vec![1, 2]);
        assert_eq!(parse_ranges("3,1,3", 10).unwrap(), vec![1, 3]);
    }

    #[test]
    fn rejects_bad() {
        assert!(parse_ranges("0", 10).is_err());
        assert!(parse_ranges("11", 10).is_err());
        assert!(parse_ranges("5-2", 10).is_err());
        assert!(parse_ranges("", 10).is_err());
        assert!(parse_ranges("abc", 10).is_err());
    }
}
