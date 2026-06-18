use anyhow::{Context, Result};
use reqwest::{
    header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, USER_AGENT},
    Client,
};
use serde_json::Value;

pub async fn fetch_specs(version: &str, token: Option<&str>) -> Result<Vec<(String, Value)>> {
    let client = build_client(token)?;
    let files = list_spec_files(&client, version).await?;
    println!("Found {} spec files", files.len());

    let mut specs = Vec::new();
    for (name, url) in files {
        println!("  Downloading {name}...");
        let content = download_file(&client, &url)
            .await
            .with_context(|| format!("downloading {name}"))?;
        specs.push((name, content));
    }
    Ok(specs)
}

fn build_client(token: Option<&str>) -> Result<Client> {
    let mut headers = HeaderMap::new();
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static("kubernetools-docgen/0.1"),
    );
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/vnd.github.v3+json"),
    );
    if let Some(t) = token {
        let val = HeaderValue::from_str(&format!("Bearer {t}")).context("invalid token")?;
        headers.insert(AUTHORIZATION, val);
    }
    Client::builder()
        .default_headers(headers)
        .build()
        .context("building HTTP client")
}

async fn list_spec_files(client: &Client, version: &str) -> Result<Vec<(String, String)>> {
    let url = format!("https://api.github.com/repos/kubernetools/specs/contents/specs/{version}");
    let resp: Value = client
        .get(&url)
        .send()
        .await
        .context("listing spec files")?
        .error_for_status()
        .context("GitHub API error")?
        .json()
        .await
        .context("parsing directory listing")?;

    let arr = resp
        .as_array()
        .context("expected JSON array from GitHub Contents API")?;

    let files = arr
        .iter()
        .filter_map(|item| {
            let name = item["name"].as_str()?.to_string();
            let download_url = item["download_url"].as_str()?.to_string();
            if name.ends_with("_openapi.json") {
                Some((name, download_url))
            } else {
                None
            }
        })
        .collect();
    Ok(files)
}

async fn download_file(client: &Client, url: &str) -> Result<Value> {
    client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
        .context("parsing JSON")
}
