use anyhow::Result;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

/// Writes (or updates) sitemap.xml.
///
/// Existing entries whose URL starts with any of `evict_prefixes` are removed
/// first — this replaces the full set of URLs for the version(s) being
/// regenerated. Entries for other versions are preserved.
pub fn generate(urls: &[String], out_path: &Path, evict_prefixes: &[String]) -> Result<()> {
    let mut all_urls: BTreeSet<String> = BTreeSet::new();
    if out_path.exists() {
        for line in fs::read_to_string(out_path)?.lines() {
            let trimmed = line.trim();
            if let Some(inner) = trimmed.strip_prefix("<loc>").and_then(|s| s.strip_suffix("</loc>")) {
                if !evict_prefixes.iter().any(|p| inner.starts_with(p.as_str())) {
                    all_urls.insert(inner.to_string());
                }
            }
        }
    }
    for url in urls {
        all_urls.insert(url.clone());
    }

    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n",
    );
    for url in &all_urls {
        xml.push_str(&format!(
            "  <url>\n    <loc>{url}</loc>\n    <changefreq>weekly</changefreq>\n  </url>\n"
        ));
    }
    xml.push_str("</urlset>\n");
    fs::write(out_path, xml)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> String { s.to_string() }

    fn loc_urls(path: &Path) -> Vec<String> {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .filter_map(|l| {
                let t = l.trim();
                t.strip_prefix("<loc>")?.strip_suffix("</loc>").map(str::to_string)
            })
            .collect()
    }

    #[test]
    fn creates_sitemap_from_scratch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sitemap.xml");
        generate(&[p("https://example.com/a/"), p("https://example.com/b/")], &path, &[]).unwrap();
        assert_eq!(loc_urls(&path), ["https://example.com/a/", "https://example.com/b/"]);
    }

    #[test]
    fn merges_different_versions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sitemap.xml");
        generate(&[p("https://example.com/docs/v1.33/pod/")], &path, &[p("https://example.com/docs/v1.33/")]).unwrap();
        generate(&[p("https://example.com/docs/v1.34/pod/")], &path, &[p("https://example.com/docs/v1.34/")]).unwrap();
        let got = loc_urls(&path);
        assert_eq!(got.len(), 2);
        assert!(got.contains(&p("https://example.com/docs/v1.33/pod/")));
        assert!(got.contains(&p("https://example.com/docs/v1.34/pod/")));
    }

    #[test]
    fn evicts_stale_urls_for_regenerated_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sitemap.xml");
        let prefix = vec![p("https://example.com/docs/v1.33/")];
        generate(&[p("https://example.com/docs/v1.33/foo/"), p("https://example.com/docs/v1.33/bar/")], &path, &prefix).unwrap();
        // Second run: bar was removed from the spec
        generate(&[p("https://example.com/docs/v1.33/foo/")], &path, &prefix).unwrap();
        assert_eq!(loc_urls(&path), ["https://example.com/docs/v1.33/foo/"]);
    }

    #[test]
    fn eviction_does_not_affect_other_versions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sitemap.xml");
        generate(&[p("https://example.com/docs/v1.33/foo/"), p("https://example.com/docs/v1.34/foo/")], &path, &[]).unwrap();
        generate(&[p("https://example.com/docs/v1.33/bar/")], &path, &[p("https://example.com/docs/v1.33/")]).unwrap();
        let got = loc_urls(&path);
        assert!(got.contains(&p("https://example.com/docs/v1.34/foo/")), "v1.34 must survive");
        assert!(!got.contains(&p("https://example.com/docs/v1.33/foo/")), "stale v1.33 entry must be gone");
        assert!(got.contains(&p("https://example.com/docs/v1.33/bar/")), "new v1.33 entry must be present");
    }

    #[test]
    fn deduplicates_urls() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sitemap.xml");
        let prefix = vec![p("https://example.com/")];
        generate(&[p("https://example.com/a/")], &path, &prefix).unwrap();
        generate(&[p("https://example.com/a/")], &path, &prefix).unwrap();
        assert_eq!(loc_urls(&path).len(), 1);
    }
}
