use std::{
    fs::File,
    io::{BufRead, BufReader, Read},
    net::IpAddr,
};

use anyhow::{Error, Ok as _Ok, Result};
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use serde::{Deserialize, Serialize};
use ureq::{
    RequestBuilder,
    typestate::{WithBody, WithoutBody},
};

const CONFIG_FILE: &str = "config.toml";
const CF_TRACE_URL: &str = "https://cloudflare.com/cdn-cgi/trace";
const CF_API_BASE_URL: &str = "https://api.cloudflare.com/client/v4";

#[derive(Deserialize)]
struct Config {
    dns_records: Vec<String>,
    cf_api_key: String,
    cf_zone_id: String,
}

struct ApiClient {
    cf_api_key: String,
}

impl ApiClient {
    fn new(config: &Config) -> Self {
        Self {
            cf_api_key: config.cf_api_key.clone(),
        }
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
}

#[derive(Deserialize)]
struct DnsRecordsListResponse {
    result: Vec<DnsRecordsListResult>,
}

#[derive(Deserialize)]
struct DnsRecordsListResult {
    id: String,
    name: String,
    #[serde(rename = "type")]
    type_: String,
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

    fn into_json(self) -> serde_json::Result<String> {
        serde_json::to_string(&self)
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
    let client = ApiClient::new(&config);
    let results: Vec<Result<()>> = config
        .dns_records
        .par_iter()
        .map(|dns_record| update_dns_record(&client, &config, public_ip, dns_record))
        .collect();
    for result in results {
        result?;
    }
    _Ok(())
}

fn update_dns_record(
    client: &ApiClient,
    config: &Config,
    public_ip: IpAddr,
    dns_record: &str,
) -> Result<()> {
    let dns_base_url = format!("/zones/{}/dns_records", config.cf_zone_id);
    let record_type = get_record_type(&public_ip);
    let request_body = DnsRecord::new(dns_record, &public_ip).into_json().unwrap();
    // Get list of records
    let records = {
        let reader = client
            .get(&dns_base_url)
            .query("type", record_type)
            .query("name", dns_record)
            .call()?
            .into_body()
            .into_reader();
        let response: DnsRecordsListResponse = serde_json::from_reader(reader)?;
        response.result
    };
    let record_id = records
        .into_iter()
        .filter(|r| r.name == dns_record && r.type_ == record_type)
        .map(|r| r.id)
        .next();
    if let Some(record_id) = record_id {
        // Update existing DNS record
        client
            .patch([dns_base_url, record_id].join("/"))
            .send(request_body)?;
        println!("DNS record {} has been updated", dns_record);
    } else {
        // Create new DNS record
        client.post(&dns_base_url).send(request_body)?;
        println!("DNS record {} has been created", dns_record);
    }
    Ok(())
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
    let mut file = File::open(CONFIG_FILE)?;
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
