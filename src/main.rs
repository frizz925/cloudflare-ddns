use std::{
    collections::HashMap,
    env::var,
    fs::{File, exists},
    io::{BufRead, BufReader, Read},
    net::IpAddr,
};

use anyhow::{Error, Ok as _Ok, Result};
use serde::{Deserialize, Serialize};
use ureq::{
    RequestBuilder,
    typestate::{WithBody, WithoutBody},
};

const CONFIG_FILE: &str = "config.toml";
const SYSTEM_CONFIG_FILE: &str = "/etc/cloudflare-ddns.toml";

const CF_TRACE_URL: &str = "https://cloudflare.com/cdn-cgi/trace";
const CF_API_BASE_URL: &str = "https://api.cloudflare.com/client/v4";

#[derive(Deserialize)]
struct Config {
    dns_records: Vec<String>,
    cf_api_key: String,
    cf_zone_id: String,
}

#[derive(Deserialize)]
struct ListApiResponse<R> {
    result: Vec<R>,
}

#[derive(Deserialize)]
struct DnsRecordsListResult {
    id: String,
    name: String,
    #[serde(rename = "type")]
    type_: String,
    content: Option<String>,
}

#[derive(Serialize)]
struct DnsRecord {
    name: String,
    ttl: u32,
    #[serde(rename = "type")]
    type_: &'static str,
    content: String,
    proxied: bool,
}

impl DnsRecord {
    fn new(name: &str, ip_addr: &IpAddr) -> Self {
        Self {
            name: name.to_string(),
            ttl: 1,
            type_: get_record_type(ip_addr),
            content: ip_addr.to_string(),
            proxied: false,
        }
    }

    fn as_json(&self) -> serde_json::Result<String> {
        serde_json::to_string(self)
    }
}

#[derive(Clone)]
struct ApiClient {
    cf_api_key: String,
}

impl ApiClient {
    fn new(cf_api_key: String) -> Self {
        Self { cf_api_key }
    }

    fn get<P>(&self, path: P) -> RequestBuilder<WithoutBody>
    where
        P: ToString,
    {
        self.inject_headers(ureq::get(Self::build_url(path)))
    }

    fn post<P>(&self, path: P) -> RequestBuilder<WithBody>
    where
        P: ToString,
    {
        self.inject_headers(ureq::post(Self::build_url(path)))
            .content_type("application/json")
    }

    fn patch<P>(&self, path: P) -> RequestBuilder<WithBody>
    where
        P: ToString,
    {
        self.inject_headers(ureq::patch(Self::build_url(path)))
            .content_type("application/json")
    }

    fn inject_headers<B>(&self, builder: RequestBuilder<B>) -> RequestBuilder<B> {
        builder.header("Authorization", format!("Bearer {}", self.cf_api_key))
    }

    fn build_url<P>(path: P) -> String
    where
        P: ToString,
    {
        format!("{}/{}", CF_API_BASE_URL, path.to_string().trim_matches('/'))
    }

    fn dns_records_api<S>(&self, zone_id: S) -> DnsRecordsApi
    where
        S: ToString,
    {
        DnsRecordsApi {
            inner: self.clone(),
            zone_id: zone_id.to_string(),
        }
    }
}

struct DnsRecordsApi {
    inner: ApiClient,
    zone_id: String,
}

impl DnsRecordsApi {
    fn _get<P>(&self, path: P) -> RequestBuilder<WithoutBody>
    where
        P: ToString,
    {
        self.inner.get(self.build_path(path))
    }

    fn _post<P>(&self, path: P) -> RequestBuilder<WithBody>
    where
        P: ToString,
    {
        self.inner.post(self.build_path(path))
    }

    fn _patch<P>(&self, path: P) -> RequestBuilder<WithBody>
    where
        P: ToString,
    {
        self.inner.patch(self.build_path(path))
    }

    fn list(&self) -> Result<Vec<DnsRecordsListResult>> {
        let reader = self._get("").call()?.into_body().into_reader();
        let response: ListApiResponse<DnsRecordsListResult> = serde_json::from_reader(reader)?;
        Ok(response.result)
    }

    fn create(&self, record: &DnsRecord) -> Result<()> {
        self._post("").send(record.as_json().unwrap())?;
        Ok(())
    }

    fn update<S>(&self, id: S, record: &DnsRecord) -> Result<()>
    where
        S: ToString,
    {
        self._patch(id).send(record.as_json().unwrap())?;
        Ok(())
    }

    fn build_path<P>(&self, path: P) -> String
    where
        P: ToString,
    {
        format!("zones/{}/dns_records/{}", self.zone_id, path.to_string())
    }
}

fn main() -> Result<()> {
    let config = load_config()?;
    let public_ip = if let Some(ip) = get_public_ip()? {
        ip
    } else {
        return Err(Error::msg("Public IP not found"));
    };
    println!("Public IP: {}", public_ip);

    let client = ApiClient::new(config.cf_api_key).dns_records_api(config.cf_zone_id);
    let records: HashMap<_, _> = client
        .list()?
        .into_iter()
        .map(|r| (record_key(&r.name, &r.type_), r))
        .collect();
    let results = config
        .dns_records
        .into_iter()
        .flat_map(|name| {
            let request = DnsRecord::new(&name, &public_ip);
            if let Some(record) = records.get(&record_key(&name, request.type_)) {
                if let Some(content) = record.content.as_deref() {
                    if content
                        .parse()
                        .map(|ip: IpAddr| ip == public_ip)
                        .unwrap_or_default()
                    {
                        // Record content matches, do nothing
                        println!("DNS record {} matches", name);
                        return None;
                    }
                }
                // Record exists, update existing
                let record_id = record.id.to_owned();
                return Some((Some(record_id), request));
            }
            // Record doesn't exist, create a new one
            Some((None, request))
        })
        .map(|(record_id, record)| {
            if let Some(id) = record_id {
                client.update(id, &record).map(|_| {
                    println!("DNS record {} has been updated", record.name);
                })
            } else {
                client.create(&record).map(|_| {
                    println!("DNS record {} has been created", record.name);
                })
            }
        });
    for result in results {
        result?;
    }

    _Ok(())
}

fn record_key(name: &str, type_: &str) -> String {
    format!("{name}:{type_}")
}

fn get_public_ip() -> Result<Option<IpAddr>> {
    let body = ureq::get(CF_TRACE_URL).call()?.into_body().into_reader();
    let reader = BufReader::new(body);
    let mut ip_addr = None;
    for result in reader.lines() {
        let line = result?;
        if !line.starts_with("ip=") {
            continue;
        }
        ip_addr = Some(line[3..].trim().parse()?);
    }
    Ok(ip_addr)
}

fn load_config() -> Result<Config> {
    let config_path = [
        var("CONFIG_FILE").ok(),
        Some(CONFIG_FILE.to_string()),
        Some(SYSTEM_CONFIG_FILE.to_string()),
    ]
    .into_iter()
    .flatten()
    .filter(|path| exists(path).unwrap())
    .next();

    let mut file = if let Some(path) = config_path {
        File::open(path)?
    } else {
        return Err(Error::msg("Config file not found"));
    };
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    let config = toml::from_slice(&buf)?;
    Ok(config)
}

fn get_record_type(ip_addr: &IpAddr) -> &'static str {
    match ip_addr {
        IpAddr::V4(_) => "A",
        IpAddr::V6(_) => "AAAA",
    }
}
