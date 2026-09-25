//! Local Kafka operations for the Bug Days browser client. Secrets remain in this process.
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use http::StatusCode;
use rand::random;
use rdkafka::config::ClientConfig;
use rdkafka::consumer::{BaseConsumer, CommitMode, Consumer};
use rdkafka::message::{Header, Headers, Message, OwnedHeaders};
use rdkafka::producer::{FutureProducer, FutureRecord};
use rdkafka::{Offset, TopicPartitionList};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const TIMEOUT: Duration = Duration::from_secs(10);
const SESSION_TTL: Duration = Duration::from_secs(30 * 60);
const PLAN_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_PAGE: usize = 200;
const MAX_RECORD: usize = 16 * 1024 * 1024;

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

fn bad(message: impl Into<String>) -> ApiError {
    ApiError {
        status: StatusCode::BAD_REQUEST,
        message: message.into(),
    }
}

fn gateway(message: impl Into<String>) -> ApiError {
    ApiError {
        status: StatusCode::BAD_GATEWAY,
        message: message.into(),
    }
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Connection {
    brokers: String,
    #[serde(default = "plain")]
    security_protocol: String,
    sasl_mechanism: Option<String>,
    username: Option<String>,
    password: Option<String>,
    ca_pem: Option<String>,
    certificate_pem: Option<String>,
    key_pem: Option<String>,
    key_password: Option<String>,
}

fn plain() -> String {
    "PLAINTEXT".into()
}

impl Connection {
    fn validate(&self) -> Result<(), ApiError> {
        if self.brokers.is_empty() || self.brokers.len() > 2048 || self.brokers.contains('\n') {
            return Err(bad("Enter one or more Kafka bootstrap servers."));
        }
        if !["PLAINTEXT", "SSL", "SASL_PLAINTEXT", "SASL_SSL"]
            .contains(&self.security_protocol.as_str())
        {
            return Err(bad("Choose a supported Kafka security protocol."));
        }
        if self.security_protocol.starts_with("SASL") {
            if !["PLAIN", "SCRAM-SHA-256", "SCRAM-SHA-512"]
                .contains(&self.sasl_mechanism.as_deref().unwrap_or(""))
            {
                return Err(bad(
                    "Choose PLAIN, SCRAM-SHA-256, or SCRAM-SHA-512 authentication.",
                ));
            }
            if self.username.as_deref().unwrap_or("").is_empty()
                || self.password.as_deref().unwrap_or("").is_empty()
            {
                return Err(bad("Enter both SASL username and password."));
            }
        }
        Ok(())
    }

    fn client_config(&self) -> ClientConfig {
        let mut config = ClientConfig::new();
        config
            .set("bootstrap.servers", &self.brokers)
            .set("security.protocol", &self.security_protocol)
            .set("socket.timeout.ms", "10000")
            .set("client.id", "bugdays-local-kafka");
        if let Some(value) = &self.sasl_mechanism {
            config.set("sasl.mechanism", value);
        }
        if let Some(value) = &self.username {
            config.set("sasl.username", value);
        }
        if let Some(value) = &self.password {
            config.set("sasl.password", value);
        }
        if let Some(value) = &self.ca_pem {
            config.set("ssl.ca.pem", value);
        }
        if let Some(value) = &self.certificate_pem {
            config.set("ssl.certificate.pem", value);
        }
        if let Some(value) = &self.key_pem {
            config.set("ssl.key.pem", value);
        }
        if let Some(value) = &self.key_password {
            config.set("ssl.key.password", value);
        }
        config
    }
}

struct Session {
    origin: String,
    connection: Connection,
    touched: Instant,
}

#[derive(Clone)]
struct PlannedPartition {
    partition: i32,
    old: Option<i64>,
    target: i64,
}

struct ResetPlan {
    origin: String,
    session_token: String,
    group: String,
    topic: String,
    partitions: Vec<PlannedPartition>,
    created: Instant,
}

static SESSIONS: OnceLock<Mutex<HashMap<String, Session>>> = OnceLock::new();
static PLANS: OnceLock<Mutex<HashMap<String, ResetPlan>>> = OnceLock::new();

fn sessions() -> &'static Mutex<HashMap<String, Session>> {
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}
fn plans() -> &'static Mutex<HashMap<String, ResetPlan>> {
    PLANS.get_or_init(|| Mutex::new(HashMap::new()))
}
fn token() -> String {
    URL_SAFE_NO_PAD.encode(random::<[u8; 32]>())
}

fn session(token: &str, origin: &str) -> Result<Connection, ApiError> {
    let mut sessions = sessions()
        .lock()
        .map_err(|_| gateway("Session store unavailable."))?;
    sessions.retain(|_, value| value.touched.elapsed() < SESSION_TTL);
    let entry = sessions.get_mut(token).ok_or_else(|| ApiError {
        status: StatusCode::UNAUTHORIZED,
        message: "Kafka connection expired. Connect again.".into(),
    })?;
    if entry.origin != origin {
        return Err(bad(
            "This Kafka session belongs to a different browser origin.",
        ));
    }
    entry.touched = Instant::now();
    Ok(entry.connection.clone())
}

fn consumer(connection: &Connection, group: &str) -> Result<BaseConsumer, ApiError> {
    let mut config = connection.client_config();
    config
        .set("group.id", group)
        .set("enable.auto.commit", "false")
        .set("enable.auto.offset.store", "false")
        .set("isolation.level", "read_committed");
    config
        .create()
        .map_err(|e| gateway(format!("Kafka client: {e}")))
}

fn parse<T: for<'de> Deserialize<'de>>(body: Value) -> Result<T, ApiError> {
    serde_json::from_value(body).map_err(|_| bad("Check the Kafka request fields and try again."))
}

async fn blocking<T: Send + 'static>(
    task: impl FnOnce() -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(task)
        .await
        .map_err(|_| gateway("Kafka operation interrupted."))?
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Auth {
    token: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TopicRequest {
    token: String,
    topic: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BrowseRequest {
    token: String,
    topic: String,
    partition: i32,
    #[serde(default = "offset_mode")]
    mode: String,
    offset: Option<String>,
    time_ms: Option<i64>,
    end_time_ms: Option<i64>,
    end_exclusive: Option<String>,
    #[serde(default = "default_limit")]
    limit: usize,
}
fn offset_mode() -> String {
    "offset".into()
}
fn default_limit() -> usize {
    100
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct Record {
    topic: String,
    partition: i32,
    offset: String,
    timestamp_ms: Option<i64>,
    key_base64: Option<String>,
    value_base64: Option<String>,
    headers: Vec<RecordHeader>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct RecordHeader {
    key: String,
    value_base64: Option<String>,
}

fn record(message: &impl Message) -> Result<Record, ApiError> {
    let key = message.key().map(|b| STANDARD.encode(b));
    let value = message.payload().map(|b| STANDARD.encode(b));
    if message.payload().is_some_and(|b| b.len() > MAX_RECORD)
        || message.key().is_some_and(|b| b.len() > MAX_RECORD)
    {
        return Err(bad("A Kafka record exceeds the 16 MiB browser limit."));
    }
    let headers = message
        .headers()
        .map(|hs| {
            hs.iter()
                .map(|h| RecordHeader {
                    key: h.key.to_string(),
                    value_base64: h.value.map(|b| STANDARD.encode(b)),
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Record {
        topic: message.topic().into(),
        partition: message.partition(),
        offset: message.offset().to_string(),
        timestamp_ms: message.timestamp().to_millis(),
        key_base64: key,
        value_base64: value,
        headers,
    })
}

fn parse_offset(raw: &str) -> Result<i64, ApiError> {
    raw.parse::<i64>()
        .ok()
        .filter(|v| *v >= 0)
        .ok_or_else(|| bad("Offset must be a nonnegative whole number."))
}

fn timestamp_offset(
    consumer: &BaseConsumer,
    topic: &str,
    partition: i32,
    ms: i64,
    latest: i64,
) -> Result<i64, ApiError> {
    let mut list = TopicPartitionList::new();
    list.add_partition_offset(topic, partition, Offset::Offset(ms))
        .map_err(|e| gateway(e.to_string()))?;
    let result = consumer
        .offsets_for_times(list, TIMEOUT)
        .map_err(|e| gateway(e.to_string()))?;
    match result.elements().first().map(|entry| entry.offset()) {
        Some(Offset::Offset(value)) if value >= 0 => Ok(value),
        _ => Ok(latest),
    }
}

fn read_batch(connection: &Connection, req: &BrowseRequest) -> Result<Value, ApiError> {
    if req.topic.is_empty() || req.limit == 0 || req.limit > MAX_PAGE {
        return Err(bad("Choose a topic and a page size from 1 to 200."));
    }
    let consumer = consumer(connection, "bugdays-manual-read")?;
    let (earliest, latest) = consumer
        .fetch_watermarks(&req.topic, req.partition, TIMEOUT)
        .map_err(|e| gateway(e.to_string()))?;
    let start = match req.mode.as_str() {
        "earliest" => earliest,
        "latest" => latest.saturating_sub(req.limit as i64).max(earliest),
        "time" => timestamp_offset(
            &consumer,
            &req.topic,
            req.partition,
            req.time_ms.ok_or_else(|| bad("Choose a starting time."))?,
            latest,
        )?,
        "offset" => parse_offset(req.offset.as_deref().unwrap_or("0"))?.max(earliest),
        _ => return Err(bad("Choose offset, earliest, latest, or time.")),
    };
    let end = if let Some(ms) = req.end_time_ms {
        timestamp_offset(&consumer, &req.topic, req.partition, ms, latest)?
    } else {
        match &req.end_exclusive {
            Some(s) => parse_offset(s)?.min(latest),
            None => latest,
        }
    };
    let mut assignment = TopicPartitionList::new();
    assignment
        .add_partition_offset(&req.topic, req.partition, Offset::Offset(start))
        .map_err(|e| bad(e.to_string()))?;
    consumer
        .assign(&assignment)
        .map_err(|e| gateway(e.to_string()))?;
    let mut rows = Vec::new();
    let mut cursor = start;
    let mut page_bytes = 0usize;
    let deadline = Instant::now() + Duration::from_secs(5);
    while rows.len() < req.limit && cursor < end && Instant::now() < deadline {
        match consumer.poll(Duration::from_millis(150)) {
            Some(Ok(message)) => {
                if message.offset() >= end {
                    cursor = end;
                    break;
                }
                let size = message.key().map_or(0, |value| value.len())
                    + message.payload().map_or(0, |value| value.len())
                    + message.headers().map_or(0, |headers| {
                        headers
                            .iter()
                            .map(|header| {
                                header.key.len() + header.value.map_or(0, |value| value.len())
                            })
                            .sum::<usize>()
                    });
                if size > MAX_RECORD {
                    return Err(bad("A Kafka record exceeds the 16 MiB browser limit."));
                }
                if page_bytes + size > MAX_RECORD {
                    cursor = message.offset();
                    break;
                }
                page_bytes += size;
                cursor = message.offset().saturating_add(1);
                rows.push(record(&message)?);
            }
            Some(Err(error)) => return Err(gateway(error.to_string())),
            None => {}
        }
    }
    Ok(
        json!({"records": rows, "nextOffset": cursor.to_string(), "earliest": earliest.to_string(), "latest": latest.to_string(), "endExclusive": end.to_string(), "hasMore": cursor < end && !rows.is_empty()}),
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GroupRequest {
    token: String,
    group: String,
    topic: String,
}

fn group_is_idle(consumer: &BaseConsumer, group: &str) -> Result<bool, ApiError> {
    let listed = consumer
        .fetch_group_list(Some(group), TIMEOUT)
        .map_err(|e| gateway(e.to_string()))?;
    Ok(listed
        .groups()
        .iter()
        .find(|g| g.name() == group)
        .is_none_or(|g| g.members().is_empty() && ["Empty", "Dead"].contains(&g.state())))
}

fn offsets_for_group(
    consumer: &BaseConsumer,
    topic: &str,
    partitions: &[i32],
) -> Result<Vec<Option<i64>>, ApiError> {
    let mut list = TopicPartitionList::new();
    for partition in partitions {
        list.add_partition(topic, *partition);
    }
    let result = consumer
        .committed_offsets(list, TIMEOUT)
        .map_err(|e| gateway(e.to_string()))?;
    Ok(result
        .elements()
        .iter()
        .map(|e| match e.offset() {
            Offset::Offset(v) if v >= 0 => Some(v),
            _ => None,
        })
        .collect())
}

type PartitionMetadata = (i32, i32, Vec<i32>, Vec<i32>);

fn topic_partitions(
    consumer: &BaseConsumer,
    topic: &str,
) -> Result<Vec<PartitionMetadata>, ApiError> {
    let metadata = consumer
        .fetch_metadata(Some(topic), TIMEOUT)
        .map_err(|e| gateway(e.to_string()))?;
    let topic_data = metadata
        .topics()
        .iter()
        .find(|item| item.name() == topic)
        .ok_or_else(|| bad("Topic not found."))?;
    if let Some(error) = topic_data.error() {
        return Err(gateway(format!("{error:?}")));
    }
    let parts = topic_data
        .partitions()
        .iter()
        .map(|p| (p.id(), p.leader(), p.replicas().to_vec(), p.isr().to_vec()))
        .collect::<Vec<_>>();
    if parts.len() > 512 {
        return Err(bad(
            "This topic has over 512 partitions. Select a smaller scope.",
        ));
    }
    Ok(parts)
}

// Kafka consumer-protocol MemberAssignment (version, topic/partition array,
// user data). Malformed or newer encodings simply show no assignment.
fn member_assignment(bytes: &[u8]) -> HashMap<String, Vec<i32>> {
    fn take<'a>(bytes: &'a [u8], at: &mut usize, count: usize) -> Option<&'a [u8]> {
        let end = at.checked_add(count)?;
        let value = bytes.get(*at..end)?;
        *at = end;
        Some(value)
    }
    fn number(bytes: &[u8], at: &mut usize, size: usize) -> Option<i32> {
        let raw = take(bytes, at, size)?;
        Some(if size == 2 {
            i16::from_be_bytes([raw[0], raw[1]]) as i32
        } else {
            i32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]])
        })
    }
    let mut at = 0;
    let mut result = HashMap::new();
    if number(bytes, &mut at, 2).is_none() {
        return result;
    }
    let count = number(bytes, &mut at, 4).unwrap_or(-1);
    if !(0..=512).contains(&count) {
        return result;
    }
    for _ in 0..count {
        let name_len = number(bytes, &mut at, 2).unwrap_or(-1);
        if !(0..=1024).contains(&name_len) {
            return HashMap::new();
        }
        let name = match take(bytes, &mut at, name_len as usize)
            .and_then(|v| std::str::from_utf8(v).ok())
        {
            Some(value) => value.to_string(),
            None => return HashMap::new(),
        };
        let part_count = number(bytes, &mut at, 4).unwrap_or(-1);
        if !(0..=4096).contains(&part_count) {
            return HashMap::new();
        }
        let mut parts = Vec::with_capacity(part_count as usize);
        for _ in 0..part_count {
            match number(bytes, &mut at, 4) {
                Some(value) => parts.push(value),
                None => return HashMap::new(),
            }
        }
        result.insert(name, parts);
    }
    result
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResetRequest {
    token: String,
    group: String,
    topic: String,
    partitions: Vec<i32>,
    mode: String,
    value: Option<String>,
    targets: Option<Vec<ResetTarget>>,
}
#[derive(Deserialize)]
struct ResetTarget {
    partition: i32,
    offset: String,
}

fn reset_preview(
    connection: &Connection,
    req: ResetRequest,
    origin: &str,
) -> Result<Value, ApiError> {
    if req.group.is_empty()
        || req.topic.is_empty()
        || req.partitions.is_empty()
        || req.partitions.len() > 512
    {
        return Err(bad("Choose a group, topic, and up to 512 partitions."));
    }
    let consumer = consumer(connection, &req.group)?;
    if !group_is_idle(&consumer, &req.group)? {
        return Err(bad(
            "Stop this consumer group before changing its offsets, then refresh.",
        ));
    }
    let known = topic_partitions(&consumer, &req.topic)?;
    let mut parts = req.partitions.clone();
    parts.sort_unstable();
    parts.dedup();
    if parts.iter().any(|p| !known.iter().any(|k| k.0 == *p)) {
        return Err(bad("A selected partition is not in the topic."));
    }
    let committed = offsets_for_group(&consumer, &req.topic, &parts)?;
    let mut planned = Vec::new();
    let mut output = Vec::new();
    for (index, partition) in parts.iter().enumerate() {
        let (low, high) = consumer
            .fetch_watermarks(&req.topic, *partition, TIMEOUT)
            .map_err(|e| gateway(e.to_string()))?;
        let old = committed[index];
        let raw = req.value.as_deref().unwrap_or("");
        let target = match req.mode.as_str() {
            "to-offset" => parse_offset(raw)?,
            "to-earliest" => low,
            "to-latest" => high,
            "to-current" => old.unwrap_or(high),
            "shift-by" => old
                .ok_or_else(|| bad("Shift by requires an existing committed offset."))?
                .checked_add(
                    raw.parse::<i64>()
                        .map_err(|_| bad("Enter a whole-number offset shift."))?,
                )
                .ok_or_else(|| bad("Offset shift is too large."))?,
            "to-datetime" => timestamp_offset(
                &consumer,
                &req.topic,
                *partition,
                raw.parse::<i64>()
                    .map_err(|_| bad("Enter a valid UTC time."))?,
                high,
            )?,
            "by-duration" => {
                let ms = raw
                    .parse::<i64>()
                    .ok()
                    .filter(|v| *v >= 0)
                    .ok_or_else(|| bad("Enter a positive duration in milliseconds."))?;
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|_| gateway("Clock unavailable."))?
                    .as_millis() as i64;
                timestamp_offset(
                    &consumer,
                    &req.topic,
                    *partition,
                    now.saturating_sub(ms),
                    high,
                )?
            }
            "from-file" => parse_offset(
                &req.targets
                    .as_ref()
                    .and_then(|t| t.iter().find(|t| t.partition == *partition))
                    .ok_or_else(|| bad("CSV is missing a selected partition."))?
                    .offset,
            )?,
            _ => return Err(bad("Choose a supported offset reset action.")),
        };
        if target < low || target > high {
            return Err(bad(format!(
                "Partition {partition}: target must be between {low} and {high}."
            )));
        }
        planned.push(PlannedPartition {
            partition: *partition,
            old,
            target,
        });
        output.push(json!({"partition": partition, "current": old.map(|v| v.to_string()), "target": target.to_string(), "earliest": low.to_string(), "latest": high.to_string()}));
    }
    let plan_id = token();
    let mut plans = plans()
        .lock()
        .map_err(|_| gateway("Plan store unavailable."))?;
    plans.retain(|_, plan| plan.created.elapsed() < PLAN_TTL);
    plans.insert(
        plan_id.clone(),
        ResetPlan {
            origin: origin.into(),
            session_token: req.token,
            group: req.group,
            topic: req.topic,
            partitions: planned,
            created: Instant::now(),
        },
    );
    Ok(json!({"planId": plan_id, "expiresInSeconds": 300, "partitions": output}))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApplyRequest {
    token: String,
    plan_id: String,
}

fn reset_apply(
    connection: &Connection,
    req: ApplyRequest,
    origin: &str,
) -> Result<Value, ApiError> {
    let plan = plans()
        .lock()
        .map_err(|_| gateway("Plan store unavailable."))?
        .remove(&req.plan_id)
        .ok_or_else(|| bad("Offset preview expired. Preview it again."))?;
    if plan.created.elapsed() >= PLAN_TTL
        || plan.origin != origin
        || plan.session_token != req.token
    {
        return Err(bad("Offset preview expired. Preview it again."));
    }
    let consumer = consumer(connection, &plan.group)?;
    if !group_is_idle(&consumer, &plan.group)? {
        return Err(bad("The group is active now. Stop it and preview again."));
    }
    let partitions = plan
        .partitions
        .iter()
        .map(|p| p.partition)
        .collect::<Vec<_>>();
    let current = offsets_for_group(&consumer, &plan.topic, &partitions)?;
    for (index, part) in plan.partitions.iter().enumerate() {
        let (low, high) = consumer
            .fetch_watermarks(&plan.topic, part.partition, TIMEOUT)
            .map_err(|e| gateway(e.to_string()))?;
        if current[index] != part.old || part.target < low || part.target > high {
            return Err(bad(
                "Offsets or retention changed since preview. Preview again.",
            ));
        }
    }
    let mut list = TopicPartitionList::new();
    for part in &plan.partitions {
        list.add_partition_offset(&plan.topic, part.partition, Offset::Offset(part.target))
            .map_err(|e| bad(e.to_string()))?;
    }
    consumer
        .commit(&list, CommitMode::Sync)
        .map_err(|e| gateway(format!("Kafka did not accept the offset change: {e}")))?;
    let actual = offsets_for_group(&consumer, &plan.topic, &partitions)?;
    let result = plan.partitions.iter().enumerate().map(|(i, p)| json!({"partition": p.partition, "previous": p.old.map(|v| v.to_string()), "requested": p.target.to_string(), "actual": actual[i].map(|v| v.to_string()), "ok": actual[i] == Some(p.target)})).collect::<Vec<_>>();
    Ok(json!({"group": plan.group, "topic": plan.topic, "partitions": result}))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReplayRequest {
    source_token: String,
    destination_token: String,
    source_topic: String,
    destination_topic: String,
    partition: i32,
    offset: String,
    end_exclusive: Option<String>,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default = "original_policy")]
    partition_policy: String,
    fixed_partition: Option<i32>,
    #[serde(default)]
    preserve_timestamp: bool,
    edit: Option<EditableRecord>,
}
fn original_policy() -> String {
    "original".into()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EditableRecord {
    key_base64: Option<String>,
    value_base64: Option<String>,
    headers: Vec<RecordHeader>,
}

fn decode_opt(value: &Option<String>) -> Result<Option<Vec<u8>>, ApiError> {
    value
        .as_ref()
        .map(|s| {
            STANDARD
                .decode(s)
                .map_err(|_| bad("Invalid Base64 record bytes."))
        })
        .transpose()
}

async fn replay(
    source: Connection,
    destination: Connection,
    req: ReplayRequest,
) -> Result<Value, ApiError> {
    if req.limit == 0
        || req.limit > MAX_PAGE
        || req.source_topic.is_empty()
        || req.destination_topic.is_empty()
    {
        return Err(bad("Choose topics and up to 200 messages per batch."));
    }
    if !["original", "fixed", "key"].contains(&req.partition_policy.as_str()) {
        return Err(bad("Choose original, fixed, or key partitioning."));
    }
    if req.partition_policy == "fixed" && req.fixed_partition.is_none() {
        return Err(bad("Choose a destination partition."));
    }
    let start = parse_offset(&req.offset)?;
    let browse = BrowseRequest {
        token: String::new(),
        topic: req.source_topic.clone(),
        partition: req.partition,
        mode: "offset".into(),
        offset: Some(start.to_string()),
        time_ms: None,
        end_time_ms: None,
        end_exclusive: req
            .end_exclusive
            .clone()
            .or_else(|| Some(start.saturating_add(req.limit as i64).to_string())),
        limit: req.limit,
    };
    let page = blocking(move || read_batch(&source, &browse)).await?;
    let mut records: Vec<Record> = serde_json::from_value(page["records"].clone())
        .map_err(|_| gateway("Could not read the selected messages."))?;
    if let Some(edit) = req.edit {
        if records.len() != 1 {
            return Err(bad("Edit is available for one selected message only."));
        }
        decode_opt(&edit.key_base64)?;
        decode_opt(&edit.value_base64)?;
        records[0].key_base64 = edit.key_base64;
        records[0].value_base64 = edit.value_base64;
        records[0].headers = edit.headers;
    }
    let mut producer_config = destination.client_config();
    producer_config
        .set("enable.idempotence", "true")
        .set("partitioner", "murmur2_random")
        .set("message.timeout.ms", "30000")
        .set("acks", "all");
    let producer: FutureProducer = producer_config
        .create()
        .map_err(|e| gateway(format!("Kafka producer: {e}")))?;
    let mut deliveries = Vec::new();
    for row in &records {
        let key = decode_opt(&row.key_base64)?;
        let value = decode_opt(&row.value_base64)?;
        if key.as_ref().is_some_and(|v| v.len() > MAX_RECORD)
            || value.as_ref().is_some_and(|v| v.len() > MAX_RECORD)
        {
            return Err(bad("A record exceeds the 16 MiB limit."));
        }
        let mut headers = OwnedHeaders::new();
        for header in &row.headers {
            let decoded = decode_opt(&header.value_base64)?;
            headers = headers.insert(Header {
                key: &header.key,
                value: decoded.as_deref(),
            });
        }
        let mut outgoing = FutureRecord::<[u8], [u8]>::to(&req.destination_topic).headers(headers);
        if let Some(ref bytes) = key {
            outgoing = outgoing.key(bytes);
        }
        if let Some(ref bytes) = value {
            outgoing = outgoing.payload(bytes);
        }
        if req.preserve_timestamp {
            if let Some(time) = row.timestamp_ms {
                outgoing = outgoing.timestamp(time);
            }
        }
        outgoing = match req.partition_policy.as_str() {
            "original" => outgoing.partition(row.partition),
            "fixed" => outgoing.partition(req.fixed_partition.unwrap()),
            _ => outgoing,
        };
        match producer.send(outgoing, Duration::from_secs(30)).await {
            Ok(delivery) => deliveries.push(json!({"sourceOffset": row.offset, "partition": delivery.partition, "offset": delivery.offset.to_string()})),
            Err((error, _)) => return Err(gateway(format!("Replay stopped after {} messages: {error}", deliveries.len()))),
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    Ok(
        json!({"sent": deliveries.len(), "deliveries": deliveries, "nextOffset": page["nextOffset"], "hasMore": page["hasMore"]}),
    )
}

pub async fn execute(path: &str, body: Value, origin: &str) -> Result<Value, ApiError> {
    match path {
        "/api/v1/kafka/connect" => {
            let config: Connection = parse(body)?;
            config.validate()?;
            let check = config.clone();
            let overview = blocking(move || {
                let consumer = consumer(&check, "bugdays-connect")?;
                let metadata = consumer
                    .fetch_metadata(None, TIMEOUT)
                    .map_err(|e| gateway(format!("Could not connect to Kafka: {e}")))?;
                Ok(json!({"brokers": metadata.brokers().len(), "topics": metadata.topics().len()}))
            })
            .await?;
            let id = token();
            let mut sessions = sessions()
                .lock()
                .map_err(|_| gateway("Session store unavailable."))?;
            sessions.retain(|_, session| session.touched.elapsed() < SESSION_TTL);
            if sessions.len() >= 8 {
                return Err(bad("This bridge already has eight Kafka connections. Restart it to clear unused connections."));
            }
            sessions.insert(
                id.clone(),
                Session {
                    origin: origin.into(),
                    connection: config,
                    touched: Instant::now(),
                },
            );
            Ok(json!({"token": id, "expiresInSeconds": 1800, "overview": overview}))
        }
        "/api/v1/kafka/disconnect" => {
            let req: Auth = parse(body)?;
            let _ = session(&req.token, origin)?;
            sessions()
                .lock()
                .map_err(|_| gateway("Session store unavailable."))?
                .remove(&req.token);
            Ok(json!({"disconnected": true}))
        }
        "/api/v1/kafka/metadata" => {
            let req: Auth = parse(body)?;
            let connection = session(&req.token, origin)?;
            blocking(move || {
                let consumer = consumer(&connection, "bugdays-metadata")?;
                let metadata = consumer.fetch_metadata(None, TIMEOUT).map_err(|e| gateway(e.to_string()))?;
                Ok(json!({"brokers": metadata.brokers().iter().map(|b| json!({"id": b.id(), "host": b.host(), "port": b.port()})).collect::<Vec<_>>(),
                    "topics": metadata.topics().iter().filter(|t| !t.name().starts_with("__")).map(|t| json!({"name": t.name(), "partitions": t.partitions().len()})).collect::<Vec<_>>() }))
            }).await
        }
        "/api/v1/kafka/topic" => {
            let req: TopicRequest = parse(body)?;
            let connection = session(&req.token, origin)?;
            blocking(move || {
                let consumer = consumer(&connection, "bugdays-topic")?;
                let parts = topic_partitions(&consumer, &req.topic)?;
                let rows = parts.iter().map(|(id, leader, replicas, isr)| {
                    let (low, high) = consumer.fetch_watermarks(&req.topic, *id, TIMEOUT).unwrap_or((-1, -1));
                    json!({"partition": id, "leader": leader, "replicas": replicas, "isr": isr, "earliest": low.to_string(), "latest": high.to_string()})
                }).collect::<Vec<_>>();
                Ok(json!({"topic": req.topic, "partitions": rows}))
            }).await
        }
        "/api/v1/kafka/browse" => {
            let req: BrowseRequest = parse(body)?;
            let connection = session(&req.token, origin)?;
            blocking(move || read_batch(&connection, &req)).await
        }
        "/api/v1/kafka/groups" => {
            let req: Auth = parse(body)?;
            let connection = session(&req.token, origin)?;
            blocking(move || {
                let consumer = consumer(&connection, "bugdays-groups")?;
                let groups = consumer.fetch_group_list(None, TIMEOUT).map_err(|e| gateway(e.to_string()))?;
                Ok(json!({"groups": groups.groups().iter().map(|g| json!({"name": g.name(), "state": g.state(), "members": g.members().len()})).collect::<Vec<_>>()}))
            }).await
        }
        "/api/v1/kafka/group" => {
            let req: GroupRequest = parse(body)?;
            let connection = session(&req.token, origin)?;
            blocking(move || {
                let consumer = consumer(&connection, &req.group)?;
                let listed = consumer.fetch_group_list(Some(&req.group), TIMEOUT).map_err(|e| gateway(e.to_string()))?;
                let group = listed.groups().iter().find(|g| g.name() == req.group);
                let parts = topic_partitions(&consumer, &req.topic)?;
                let ids = parts.iter().map(|p| p.0).collect::<Vec<_>>();
                let commits = offsets_for_group(&consumer, &req.topic, &ids)?;
                let members = group.map(|g| g.members().iter().map(|m| {
                    let assignments = m.assignment().map(member_assignment).unwrap_or_default();
                    json!({"id": m.id(), "clientId": m.client_id(), "host": m.client_host(), "assignments": assignments})
                }).collect::<Vec<_>>()).unwrap_or_default();
                let rows = parts.iter().enumerate().map(|(i, p)| {
                    let (low, high) = consumer.fetch_watermarks(&req.topic, p.0, TIMEOUT).unwrap_or((-1, -1));
                    let commit = commits[i];
                    let owner = members.iter().find(|m| m["assignments"][&req.topic].as_array().is_some_and(|a| a.iter().any(|v| v.as_i64() == Some(p.0 as i64)))).and_then(|m| m["id"].as_str());
                    json!({"partition": p.0, "committed": commit.map(|v| v.to_string()), "earliest": low.to_string(), "latest": high.to_string(), "lag": commit.filter(|_| high >= 0).map(|v| high.saturating_sub(v).max(0).to_string()), "leader": p.1, "replicas": p.2, "isr": p.3, "owner": owner})
                }).collect::<Vec<_>>();
                Ok(json!({"group": req.group, "topic": req.topic, "state": group.map(|g| g.state()).unwrap_or("Unknown"), "members": members, "partitions": rows, "sampledAt": SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis()}))
            }).await
        }
        "/api/v1/kafka/reset-preview" => {
            let req: ResetRequest = parse(body)?;
            let connection = session(&req.token, origin)?;
            let origin = origin.to_string();
            blocking(move || reset_preview(&connection, req, &origin)).await
        }
        "/api/v1/kafka/reset-apply" => {
            let req: ApplyRequest = parse(body)?;
            let connection = session(&req.token, origin)?;
            let origin = origin.to_string();
            blocking(move || reset_apply(&connection, req, &origin)).await
        }
        "/api/v1/kafka/replay" => {
            let req: ReplayRequest = parse(body)?;
            let source = session(&req.source_token, origin)?;
            let destination = session(&req.destination_token, origin)?;
            replay(source, destination, req).await
        }
        _ => Err(ApiError {
            status: StatusCode::NOT_FOUND,
            message: "Unknown Kafka operation.".into(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn member_assignment_reads_partition_ownership_and_rejects_truncation() {
        let encoded = [
            0, 0, // version
            0, 0, 0, 1, // one topic
            0, 6, b'o', b'r', b'd', b'e', b'r', b's', 0, 0, 0, 2, // two partitions
            0, 0, 0, 0, 0, 0, 0, 3,
        ];
        assert_eq!(member_assignment(&encoded)["orders"], vec![0, 3]);
        assert!(member_assignment(&encoded[..encoded.len() - 1]).is_empty());
    }

    #[test]
    fn connection_validation_limits_auth_modes() {
        let mut config = Connection {
            brokers: "127.0.0.1:9092".into(),
            security_protocol: "PLAINTEXT".into(),
            sasl_mechanism: None,
            username: None,
            password: None,
            ca_pem: None,
            certificate_pem: None,
            key_pem: None,
            key_password: None,
        };
        assert!(config.validate().is_ok());
        config.security_protocol = "SASL_SSL".into();
        assert!(config.validate().is_err());
        config.sasl_mechanism = Some("SCRAM-SHA-512".into());
        config.username = Some("test".into());
        config.password = Some("password".into());
        assert!(config.validate().is_ok());
        config.sasl_mechanism = Some("GSSAPI".into());
        assert!(config.validate().is_err());
    }

    #[test]
    fn offsets_are_nonnegative_64_bit_integers() {
        assert_eq!(parse_offset("9223372036854775807").unwrap(), i64::MAX);
        for raw in ["-1", "1.5", "", "9223372036854775808"] {
            assert!(parse_offset(raw).is_err());
        }
    }
}
