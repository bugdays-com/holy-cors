use std::net::IpAddr;
use std::time::Instant;

use hickory_resolver::net::{DnsError, NetError};
use hickory_resolver::proto::op::ResponseCode;
use hickory_resolver::proto::rr::{Name, RData, RecordType};
use hickory_resolver::{ConnectionProvider, Resolver, TokioResolver};
use serde::{Deserialize, Serialize};

const DEFAULT_TYPES: &[&str] = &["A", "AAAA", "CNAME", "MX", "TXT", "NS", "SOA", "SRV", "CAA"];
const MAX_QUERY_TYPES: usize = 12;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DnsQueryRequest {
    pub name: String,
    #[serde(default)]
    pub types: Vec<String>,
    #[serde(default = "default_forward_check")]
    pub forward_check: bool,
}

fn default_forward_check() -> bool {
    true
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DnsQueryResponse {
    pub query: String,
    pub resolver: &'static str,
    pub reverse_lookup: bool,
    pub elapsed_ms: u128,
    pub results: Vec<DnsRecordSet>,
    pub forward_confirmed: Option<bool>,
    pub forward_addresses: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DnsRecordSet {
    pub record_type: String,
    pub status: String,
    pub elapsed_ms: u128,
    pub answers: Vec<DnsAnswer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DnsAnswer {
    pub name: String,
    #[serde(rename = "type")]
    pub record_type: String,
    pub ttl: u32,
    pub data: String,
}

pub async fn query(request: DnsQueryRequest) -> Result<DnsQueryResponse, String> {
    let query_name = validate_name(&request.name)?;
    let ip = query_name.parse::<IpAddr>().ok();
    let type_names = normalized_types(&request.types, ip.is_some())?;
    let resolver = TokioResolver::builder_tokio()
        .map_err(|error| format!("Could not read this device's DNS settings: {error}"))?
        .build()
        .map_err(|error| format!("Could not start the system DNS resolver: {error}"))?;
    query_with_resolver(&resolver, query_name, ip, type_names, request.forward_check).await
}

async fn query_with_resolver<P: ConnectionProvider>(
    resolver: &Resolver<P>,
    query_name: String,
    ip: Option<IpAddr>,
    type_names: Vec<String>,
    forward_check: bool,
) -> Result<DnsQueryResponse, String> {
    let started = Instant::now();
    let mut results = Vec::with_capacity(type_names.len());

    for type_name in type_names {
        let record_type = parse_record_type(&type_name)?;
        let record_name = if let Some(address) = ip {
            Name::from(address)
        } else {
            Name::from_utf8(&query_name).map_err(|error| format!("Invalid hostname: {error}"))?
        };
        let query_started = Instant::now();
        let outcome = resolver.lookup(record_name, record_type).await;
        let elapsed_ms = query_started.elapsed().as_millis();

        match outcome {
            Ok(lookup) => {
                let answers = lookup
                    .answers()
                    .iter()
                    .map(|record| DnsAnswer {
                        name: record.name.to_utf8(),
                        record_type: record.record_type().to_string(),
                        ttl: record.ttl,
                        data: record_data(&record.data),
                    })
                    .collect();
                results.push(DnsRecordSet {
                    record_type: type_name,
                    status: "NOERROR".to_string(),
                    elapsed_ms,
                    answers,
                    error: None,
                });
            }
            Err(error) => {
                let (status, message) = dns_error(&error);
                results.push(DnsRecordSet {
                    record_type: type_name,
                    status,
                    elapsed_ms,
                    answers: Vec::new(),
                    error: message,
                });
            }
        }
    }

    let mut forward_addresses = Vec::new();
    let mut forward_confirmed = None;
    if let Some(address) = ip.filter(|_| forward_check) {
        let hostnames: Vec<String> = results
            .iter()
            .flat_map(|result| result.answers.iter())
            .filter(|answer| answer.record_type.eq_ignore_ascii_case("PTR"))
            .map(|answer| answer.data.trim_end_matches('.').to_string())
            .collect();
        if !hostnames.is_empty() {
            for hostname in hostnames {
                if let Ok(lookup) = resolver.lookup_ip(hostname.as_str()).await {
                    for resolved in lookup.iter() {
                        let value = resolved.to_string();
                        if !forward_addresses.contains(&value) {
                            forward_addresses.push(value);
                        }
                    }
                }
            }
            forward_confirmed = Some(
                forward_addresses
                    .iter()
                    .any(|value| value == &address.to_string()),
            );
        }
    }

    Ok(DnsQueryResponse {
        query: query_name,
        resolver: "system",
        reverse_lookup: ip.is_some(),
        elapsed_ms: started.elapsed().as_millis(),
        results,
        forward_confirmed,
        forward_addresses,
    })
}

fn validate_name(value: &str) -> Result<String, String> {
    let trimmed = value.trim().trim_end_matches('.');
    if trimmed.is_empty() {
        return Err("Enter a hostname or IP address.".to_string());
    }
    if trimmed.len() > 253 || trimmed.chars().any(char::is_whitespace) {
        return Err("Enter one valid hostname or IP address.".to_string());
    }
    if trimmed.contains("://") || trimmed.contains('/') || trimmed.contains('@') {
        return Err(
            "Enter a hostname or IP address without a URL, path, or credentials.".to_string(),
        );
    }
    Ok(trimmed.to_string())
}

fn normalized_types(values: &[String], reverse: bool) -> Result<Vec<String>, String> {
    if reverse {
        return Ok(vec!["PTR".to_string()]);
    }
    let values: Vec<String> = if values.is_empty() {
        DEFAULT_TYPES
            .iter()
            .map(|value| value.to_string())
            .collect()
    } else {
        values
            .iter()
            .map(|value| value.trim().to_ascii_uppercase())
            .collect()
    };
    if values.len() > MAX_QUERY_TYPES {
        return Err(format!(
            "Choose at most {MAX_QUERY_TYPES} DNS record types."
        ));
    }
    let mut deduplicated: Vec<String> = Vec::new();
    for value in values {
        parse_record_type(&value)?;
        if !deduplicated.contains(&value) {
            deduplicated.push(value);
        }
    }
    Ok(deduplicated)
}

fn parse_record_type(value: &str) -> Result<RecordType, String> {
    match value {
        "A" => Ok(RecordType::A),
        "AAAA" => Ok(RecordType::AAAA),
        "CNAME" => Ok(RecordType::CNAME),
        "MX" => Ok(RecordType::MX),
        "TXT" => Ok(RecordType::TXT),
        "NS" => Ok(RecordType::NS),
        "SOA" => Ok(RecordType::SOA),
        "SRV" => Ok(RecordType::SRV),
        "CAA" => Ok(RecordType::CAA),
        "PTR" => Ok(RecordType::PTR),
        _ => Err(format!("Unsupported DNS record type: {value}")),
    }
}

fn record_data(data: &RData) -> String {
    match data {
        RData::TXT(value) => value
            .txt_data
            .iter()
            .map(|part| String::from_utf8_lossy(part))
            .collect::<Vec<_>>()
            .join(""),
        RData::CNAME(_) | RData::NS(_) | RData::PTR(_) => {
            data.to_string().trim_end_matches('.').to_string()
        }
        _ => data.to_string(),
    }
}

fn dns_error(error: &NetError) -> (String, Option<String>) {
    match error {
        NetError::Dns(DnsError::NoRecordsFound(no_records)) => {
            let response_code = &no_records.response_code;
            let status = match response_code {
                ResponseCode::NXDomain => "NXDOMAIN",
                ResponseCode::NoError => "NOERROR",
                code => {
                    return (
                        code.to_string().to_ascii_uppercase(),
                        Some(error.to_string()),
                    )
                }
            };
            (status.to_string(), None)
        }
        _ => ("ERROR".to_string(), Some(error.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ip_always_selects_ptr() {
        assert_eq!(
            normalized_types(&["A".to_string()], true).unwrap(),
            vec!["PTR"]
        );
    }

    #[test]
    fn dns_types_are_deduplicated_and_normalized() {
        assert_eq!(
            normalized_types(&["a".to_string(), "A".to_string(), "mx".to_string()], false).unwrap(),
            vec!["A", "MX"]
        );
    }

    #[test]
    fn urls_are_rejected() {
        assert!(validate_name("https://example.com/path").is_err());
    }
}
