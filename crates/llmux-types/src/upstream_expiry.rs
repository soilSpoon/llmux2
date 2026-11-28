//! Schema-first PR1 wire types for upstream-expiry inventory and evidence artifacts.
//!
//! This module is a parser-boundary spike. It deliberately contains no lifecycle,
//! drain, deployment, network, or probe behavior.

use serde::{de::DeserializeOwned, Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use sha2::{Digest, Sha256 as Sha256Digest};
use std::{cmp::Ordering, collections::BTreeMap, error::Error, fmt};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const SIGNATURE_DOMAIN: &[u8] = b"LLMUX2_INVENTORY_SIGNATURE_V1\0";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WireError(String);

impl WireError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for WireError {}

fn required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

pub trait WireValidate {
    fn validate(&self) -> Result<(), WireError> {
        Ok(())
    }
}

fn canonical_bytes<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, WireError> {
    Ok(to_canonical_json(value)?.into_bytes())
}

fn canonical_cmp<T: Serialize + ?Sized>(left: &T, right: &T) -> Result<Ordering, WireError> {
    Ok(canonical_bytes(left)?.cmp(&canonical_bytes(right)?))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CanonicalEntry<K, V> {
    pub key: K,
    pub value: V,
}

impl<'de, K, V> Deserialize<'de> for CanonicalEntry<K, V>
where
    K: Deserialize<'de>,
    V: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Repr<K, V> {
            key: K,
            value: V,
        }
        let Repr { key, value } = Repr::deserialize(deserializer)?;
        Ok(Self { key, value })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct CanonicalMap<K, V> {
    pub entries: Vec<CanonicalEntry<K, V>>,
}

impl<K, V> CanonicalMap<K, V> {
    pub fn new(entries: Vec<CanonicalEntry<K, V>>) -> Self {
        Self { entries }
    }
}

impl<K, V> Default for CanonicalMap<K, V> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
}

impl<'de, K, V> Deserialize<'de> for CanonicalMap<K, V>
where
    K: Deserialize<'de> + Serialize,
    V: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let entries = Vec::<CanonicalEntry<K, V>>::deserialize(deserializer)?;
        validate_entry_order(&entries).map_err(serde::de::Error::custom)?;
        Ok(Self { entries })
    }
}

fn validate_entry_order<K: Serialize, V>(
    entries: &[CanonicalEntry<K, V>],
) -> Result<(), WireError> {
    for pair in entries.windows(2) {
        let ordering = canonical_cmp(&pair[0].key, &pair[1].key)?;
        match ordering {
            Ordering::Less => {}
            Ordering::Equal => return Err(WireError::new("duplicate canonical map key")),
            Ordering::Greater => return Err(WireError::new("canonical map entries out of order")),
        }
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct CanonicalSet<T> {
    pub values: Vec<T>,
}

impl<T> CanonicalSet<T> {
    pub fn new(values: Vec<T>) -> Self {
        Self { values }
    }
}

impl<T> Default for CanonicalSet<T> {
    fn default() -> Self {
        Self { values: Vec::new() }
    }
}

impl<'de, T> Deserialize<'de> for CanonicalSet<T>
where
    T: Deserialize<'de> + Serialize,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let values = Vec::<T>::deserialize(deserializer)?;
        validate_set_order(&values, false).map_err(serde::de::Error::custom)?;
        Ok(Self { values })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct NonEmptyCanonicalSet<T> {
    pub values: Vec<T>,
}

impl<T: Serialize> NonEmptyCanonicalSet<T> {
    pub fn new(values: Vec<T>) -> Result<Self, WireError> {
        validate_set_order(&values, true)?;
        Ok(Self { values })
    }
}

impl<'de, T> Deserialize<'de> for NonEmptyCanonicalSet<T>
where
    T: Deserialize<'de> + Serialize,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let values = Vec::<T>::deserialize(deserializer)?;
        validate_set_order(&values, true).map_err(serde::de::Error::custom)?;
        Ok(Self { values })
    }
}

fn validate_set_order<T: Serialize>(values: &[T], nonempty: bool) -> Result<(), WireError> {
    if nonempty && values.is_empty() {
        return Err(WireError::new("nonempty set is empty"));
    }
    for pair in values.windows(2) {
        match canonical_cmp(&pair[0], &pair[1])? {
            Ordering::Less => {}
            Ordering::Equal => return Err(WireError::new("duplicate canonical set element")),
            Ordering::Greater => return Err(WireError::new("canonical set out of order")),
        }
    }
    Ok(())
}

macro_rules! string_brand {
    ($name:ident, $validator:ident) => {
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            pub fn parse(value: impl Into<String>) -> Result<Self, WireError> {
                let value = value.into();
                if $validator(&value) {
                    Ok(Self(value))
                } else {
                    Err(WireError::new(concat!("invalid ", stringify!($name))))
                }
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                self.0.serialize(serializer)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::parse(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

macro_rules! integer_brand {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u64);

        impl $name {
            pub fn new(value: u64) -> Result<Self, WireError> {
                if value <= MAX_SAFE_INTEGER {
                    Ok(Self(value))
                } else {
                    Err(WireError::new(concat!("invalid ", stringify!($name))))
                }
            }
            pub fn get(self) -> u64 {
                self.0
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                self.0.serialize(serializer)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = u64::deserialize(deserializer)?;
                Self::new(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

fn no_controls(value: &str) -> bool {
    !value.chars().any(|c| c == '\0' || c.is_control())
}

fn opaque_max(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && no_controls(value)
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn valid_timestamp(value: &str) -> bool {
    fn digits(value: &str, range: std::ops::Range<usize>) -> bool {
        value.len() >= range.end && value.as_bytes()[range].iter().all(u8::is_ascii_digit)
    }
    fn leap(year: u32) -> bool {
        year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
    }
    if value.len() != 30 || !value.ends_with('Z') || &value[19..20] != "." {
        return false;
    }
    if !digits(value, 0..4)
        || !digits(value, 5..7)
        || !digits(value, 8..10)
        || !digits(value, 11..13)
        || !digits(value, 14..16)
        || !digits(value, 17..19)
        || !digits(value, 20..29)
        || value.as_bytes()[4] != b'-'
        || value.as_bytes()[7] != b'-'
        || value.as_bytes()[10] != b'T'
        || value.as_bytes()[13] != b':'
        || value.as_bytes()[16] != b':'
    {
        return false;
    }
    let year: u32 = value[0..4].parse().unwrap_or(0);
    let month: u32 = value[5..7].parse().unwrap_or(0);
    let day: u32 = value[8..10].parse().unwrap_or(0);
    let hour: u32 = value[11..13].parse().unwrap_or(99);
    let minute: u32 = value[14..16].parse().unwrap_or(99);
    let second: u32 = value[17..19].parse().unwrap_or(99);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap(year) => 29,
        2 => 28,
        _ => 0,
    };
    year >= 1 && day >= 1 && day <= days && hour < 24 && minute < 60 && second < 60
}

fn valid_provider_key(value: &str) -> bool {
    opaque_max(value, 128)
        && !value
            .chars()
            .any(|c| c.is_whitespace() || c == '/' || c == '\\')
}

fn valid_hostname(value: &str) -> bool {
    if value.is_empty() || value.len() > 253 || value.ends_with('.') {
        return false;
    }
    value.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    })
}

fn valid_normalized_path(value: &str) -> bool {
    if value.is_empty() || value.len() > 2048 || !value.starts_with('/') {
        return false;
    }
    if value == "/" {
        return true;
    }
    !value.ends_with('/')
        && value
            .split('/')
            .skip(1)
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

fn valid_no_whitespace(value: &str, max: usize) -> bool {
    opaque_max(value, max) && !value.chars().any(char::is_whitespace)
}

fn valid_signature(value: &str) -> bool {
    value.len() == 136
        && value.starts_with("ed25519:")
        && value[8..]
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

string_brand!(Sha256, valid_sha256);
string_brand!(DeploymentFingerprint, valid_sha256);
string_brand!(CredentialSubjectFingerprint, valid_sha256);
string_brand!(Timestamp, valid_timestamp);
string_brand!(ProviderKey, valid_provider_key);
string_brand!(ExactProviderModelId, validate_exact_model);
string_brand!(ServerInstanceId, validate_server_instance);
string_brand!(BootId, validate_boot_id);
string_brand!(ImportRunId, validate_import_run);
string_brand!(RequestId, validate_request_id);
string_brand!(OperatorCorrectionId, validate_operator_correction);
string_brand!(RecoveryIncidentId, validate_recovery_incident);
string_brand!(OperatorIdentity, validate_operator_identity);
string_brand!(ApprovalRef, validate_approval_ref);
string_brand!(WriterInstanceId, validate_writer_instance);
string_brand!(LogicalEvidencePath, validate_logical_path);
string_brand!(EgressClass, validate_egress);
string_brand!(StableAccountKey, validate_stable_account);
string_brand!(StableQuotaScopeKey, validate_stable_quota);
string_brand!(LowercaseHostname, valid_hostname);
string_brand!(NormalizedPath, valid_normalized_path);
string_brand!(InventorySignature, valid_signature);
string_brand!(LocalCredentialRef, validate_local_credential);
string_brand!(EvidenceRefKind, validate_evidence_ref_kind);

fn validate_exact_model(value: &str) -> bool {
    opaque_max(value, 512)
}
fn validate_server_instance(value: &str) -> bool {
    opaque_max(value, 256) && !value.contains('/')
}
fn validate_boot_id(value: &str) -> bool {
    opaque_max(value, 256)
}
fn validate_import_run(value: &str) -> bool {
    opaque_max(value, 256)
}
fn validate_request_id(value: &str) -> bool {
    opaque_max(value, 512)
}
fn validate_operator_correction(value: &str) -> bool {
    valid_no_whitespace(value, 256)
}
fn validate_recovery_incident(value: &str) -> bool {
    valid_no_whitespace(value, 256)
}
fn validate_operator_identity(value: &str) -> bool {
    opaque_max(value, 256)
}
fn validate_approval_ref(value: &str) -> bool {
    opaque_max(value, 1024)
}
fn validate_writer_instance(value: &str) -> bool {
    opaque_max(value, 512)
}
fn validate_logical_path(value: &str) -> bool {
    opaque_max(value, 4096)
}
fn validate_egress(value: &str) -> bool {
    valid_no_whitespace(value, 128)
}
fn validate_stable_account(value: &str) -> bool {
    opaque_max(value, 512)
}
fn validate_stable_quota(value: &str) -> bool {
    opaque_max(value, 512)
}
fn validate_local_credential(value: &str) -> bool {
    opaque_max(value, 512)
}
fn validate_evidence_ref_kind(value: &str) -> bool {
    opaque_max(value, 128)
}

integer_brand!(ConfigGeneration);
integer_brand!(InventoryGeneration);
integer_brand!(CanonicalIndexGeneration);
integer_brand!(CatalogGeneration);
integer_brand!(LogGeneration);
integer_brand!(EvidenceLogEpoch);
integer_brand!(FileInode);
integer_brand!(Duration);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Https;

impl Serialize for Https {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        "https".serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Https {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value == "https" {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom("scheme must be https"))
        }
    }
}

macro_rules! unit_enum {
    ($name:ident { $($variant:ident),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($variant),+ }
    };
}

unit_enum!(SuspectSource {
    CatalogAbsent,
    ExactRuntime,
    CostUnconfirmed
});
unit_enum!(QuarantineSource {
    CatalogAbsent,
    ExactRuntime
});
unit_enum!(ExpiryCause {
    AbsentCatalog,
    AbsentExactRuntime,
    NotFree,
    DeadlinePassed
});
unit_enum!(CauseStatus { Active, Clearing });
unit_enum!(TransientHoldKind {
    RateLimited,
    QuotaExhausted,
    Cooldown,
    UpstreamInfra,
    AuthOrEntitlement
});
unit_enum!(RequirementState {
    Satisfied,
    Pending,
    NotRequired
});
unit_enum!(EvidenceInputKind {
    RegenerationEvidence,
    DeploymentEvidence
});
unit_enum!(EvidenceLaneDegradedReason {
    SequenceGap,
    PreviousHashMismatch,
    FutureTimestamp,
    IncompleteRequiredCoverage,
    SchemaViolation,
    MalformedCompleteRecord,
    DeferredRecordUnavailable
});
unit_enum!(LogGapReason {
    ReadIoError,
    LogFileMissing,
    LogTailTruncated,
    ByteOffsetRegressed,
    InodeChangedWithoutCheckpoint,
    RotationCheckpointMissing
});
unit_enum!(RegenerationFailureKind {
    InventoryInvalid,
    FreeOffersInvalid,
    CandidateValidationFailed,
    OutputWriteFailed,
    InternalImportError
});
unit_enum!(RegenerationFailureCoverage {
    CompleteInputLifecycleUniverse,
    UnavailableBeforeInventoryEnumeration
});
unit_enum!(RegenerationExclusionReason {
    LifecycleSuppressed,
    LegacySuppressed,
    RevokedDiscoveryCandidate,
    MappingRejected,
    NotInSourceUniverse
});
unit_enum!(DeploymentEvidenceRecordKind {
    ConfigLoad,
    Heartbeat
});
unit_enum!(DeploymentTrafficRole {
    ActiveTraffic,
    Standby,
    Unknown
});
unit_enum!(RotationChainTrust {
    TrustworthyContiguous,
    UnknownOrGapped
});
unit_enum!(EvidenceConfidence {
    ExactMachine,
    AuthoritativeCatalog,
    ProviderHeuristic,
    LegacyFlat,
    OperatorAttested,
    Unknown
});
unit_enum!(AdapterResolutionEvidenceSource {
    CatalogAccountMetadata,
    ExistingAccountMetadataEndpoint,
    ExactProviderResponseEnvelope,
    AdapterGoldenFixture
});
unit_enum!(DeploymentFingerprintAlgorithm { V1 });

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaggedWire {
    tag: String,
    payload: Value,
}

fn serialize_tagged<S, T>(tag: &str, payload: &T, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
    T: Serialize + ?Sized,
{
    use serde::ser::SerializeMap;
    let mut map = serializer.serialize_map(Some(2))?;
    map.serialize_entry("tag", tag)?;
    map.serialize_entry("payload", payload)?;
    map.end()
}

fn tagged_parts<'de, D>(deserializer: D) -> Result<(String, Value), D::Error>
where
    D: Deserializer<'de>,
{
    let tagged = TaggedWire::deserialize(deserializer)?;
    Ok((tagged.tag, tagged.payload))
}

fn null_payload<E: serde::de::Error>(payload: Value) -> Result<(), E> {
    if payload.is_null() {
        Ok(())
    } else {
        Err(E::custom("unit enum payload must be null"))
    }
}

fn decode_payload<T, E>(payload: Value) -> Result<T, E>
where
    T: DeserializeOwned,
    E: serde::de::Error,
{
    serde_json::from_value(payload).map_err(|error| E::custom(error.to_string()))
}

macro_rules! tagged_enum_impl {
    ($name:ident, units { $($unit:ident => $unit_tag:literal),* $(,)? }, payloads { $($variant:ident($payload:ty) => $tag:literal),* $(,)? }) => {
        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                match self {
                    $(Self::$unit => serialize_tagged($unit_tag, &Value::Null, serializer),)*
                    $(Self::$variant(payload) => serialize_tagged($tag, payload, serializer),)*
                }
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let (tag, payload) = tagged_parts(deserializer)?;
                match tag.as_str() {
                    $($unit_tag => { null_payload::<D::Error>(payload)?; Ok(Self::$unit) })*
                    $($tag => Ok(Self::$variant(decode_payload::<$payload, D::Error>(payload)?)),)*
                    _ => Err(serde::de::Error::custom(format!("unknown {} tag", stringify!($name)))),
                }
            }
        }
    };
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EgressPayload {
    pub egress_class: EgressClass,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderAccountPayload {
    pub upstream_account_key: StableAccountKey,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderQuotaScopePayload {
    #[serde(deserialize_with = "required_option")]
    pub upstream_account_key: Option<StableAccountKey>,
    pub quota_scope_key: StableQuotaScopeKey,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnknownKeyedPayload {
    pub subject_fingerprint: CredentialSubjectFingerprint,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CapacitySubject {
    AnonymousNetwork(EgressPayload),
    ProviderAccount(ProviderAccountPayload),
    ProviderQuotaScope(ProviderQuotaScopePayload),
    UnknownKeyed(UnknownKeyedPayload),
}
tagged_enum_impl!(CapacitySubject,
    units {},
    payloads {
        AnonymousNetwork(EgressPayload) => "anonymous_network",
        ProviderAccount(ProviderAccountPayload) => "provider_account",
        ProviderQuotaScope(ProviderQuotaScopePayload) => "provider_quota_scope",
        UnknownKeyed(UnknownKeyedPayload) => "unknown_keyed",
    }
);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointKey {
    pub scheme: Https,
    pub host: LowercaseHostname,
    pub port: u16,
    pub base_path: NormalizedPath,
}

impl WireValidate for EndpointKey {
    fn validate(&self) -> Result<(), WireError> {
        if self.port == 0 {
            Err(WireError::new("port must be 1..65535"))
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapacityIdentity {
    pub provider_key: ProviderKey,
    pub endpoint: EndpointKey,
    pub subject: CapacitySubject,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamCapacityRef {
    pub identity: CapacityIdentity,
    pub upstream_model: ExactProviderModelId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EvidenceRef {
    pub kind: EvidenceRefKind,
    pub uri_or_journal_key: String,
    pub observed_at: Timestamp,
    pub sha256: Sha256,
}

impl<'de> Deserialize<'de> for EvidenceRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Repr {
            kind: EvidenceRefKind,
            uri_or_journal_key: String,
            observed_at: Timestamp,
            sha256: Sha256,
        }
        let value = Repr::deserialize(deserializer)?;
        let parsed = Self {
            kind: value.kind,
            uri_or_journal_key: value.uri_or_journal_key,
            observed_at: value.observed_at,
            sha256: value.sha256,
        };
        parsed.validate().map_err(serde::de::Error::custom)?;
        Ok(parsed)
    }
}

fn evidence_artifact_parts(uri_or_journal_key: &str) -> Option<Vec<&str>> {
    let parts: Vec<_> = uri_or_journal_key.split(':').collect();
    if parts.len() == 6 && parts[0] == "evidence-artifact" {
        Some(parts)
    } else {
        None
    }
}

fn evidence_lane_kind(lane: &EvidenceLaneKey) -> &'static str {
    match lane {
        EvidenceLaneKey::RegenerationImporter { .. } => "regeneration_importer",
        EvidenceLaneKey::ServerDeployment { .. } => "server_deployment",
    }
}

impl WireValidate for EvidenceRef {
    fn validate(&self) -> Result<(), WireError> {
        if self.uri_or_journal_key.is_empty()
            || self.uri_or_journal_key.chars().any(char::is_control)
        {
            return Err(WireError::new("evidence uri or journal key is invalid"));
        }
        let artifact_key = self.uri_or_journal_key.starts_with("evidence-artifact:");
        if artifact_key && self.kind.as_str() != "evidence_artifact" {
            return Err(WireError::new(
                "artifact journal key has wrong evidence ref kind",
            ));
        }
        if !artifact_key {
            return Ok(());
        }
        let parts: Vec<_> = self.uri_or_journal_key.split(':').collect();
        if parts.len() != 6 || parts[0] != "evidence-artifact" {
            return Err(WireError::new("wrong evidence artifact journal key shape"));
        }
        if !matches!(parts[1], "regeneration_importer" | "server_deployment") {
            return Err(WireError::new("wrong journal lane kind"));
        }
        if !valid_sha256(parts[2]) || !valid_sha256(parts[5]) {
            return Err(WireError::new("wrong journal key hash"));
        }
        for integer in [parts[3], parts[4]] {
            if integer.is_empty()
                || !integer.bytes().all(|byte| byte.is_ascii_digit())
                || (integer.len() > 1 && integer.starts_with('0'))
                || integer
                    .parse::<u64>()
                    .ok()
                    .is_none_or(|value| value > MAX_SAFE_INTEGER)
            {
                return Err(WireError::new("wrong journal key integer"));
            }
        }
        if parts[5] != self.sha256.as_str() {
            return Err(WireError::new(
                "journal terminal hash does not match evidence sha256",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopeResolution {
    pub from: CapacitySubject,
    pub to: CapacitySubject,
    pub evidence: AdapterResolutionEvidence,
    pub resolved_at: Timestamp,
}

impl WireValidate for ScopeResolution {
    fn validate(&self) -> Result<(), WireError> {
        if self.from == self.to {
            return Err(WireError::new("scope resolution endpoints are identical"));
        }
        if self.evidence.from_subject != self.from || self.evidence.to_subject != self.to {
            return Err(WireError::new(
                "resolution evidence subjects do not match edge endpoints",
            ));
        }
        if self.evidence.shared_account_key.is_none()
            && self.evidence.shared_quota_scope_key.is_none()
        {
            return Err(WireError::new(
                "resolution evidence requires at least one shared key",
            ));
        }
        if !matches!(
            self.evidence.confidence,
            EvidenceConfidence::ExactMachine | EvidenceConfidence::AuthoritativeCatalog
        ) {
            return Err(WireError::new(
                "resolution evidence confidence cannot create an edge",
            ));
        }
        match &self.to {
            CapacitySubject::ProviderAccount(payload) => {
                if self.evidence.shared_account_key.as_ref() != Some(&payload.upstream_account_key)
                {
                    return Err(WireError::new(
                        "provider-account target does not carry the shared account key",
                    ));
                }
            }
            CapacitySubject::ProviderQuotaScope(payload) => {
                if self.evidence.shared_quota_scope_key.as_ref() != Some(&payload.quota_scope_key) {
                    return Err(WireError::new(
                        "provider-quota-scope target does not carry the shared quota key",
                    ));
                }
                if let Some(account_key) = &payload.upstream_account_key {
                    if self
                        .evidence
                        .shared_account_key
                        .as_ref()
                        .is_some_and(|shared| shared != account_key)
                    {
                        return Err(WireError::new(
                            "provider-quota-scope target carries a conflicting account key",
                        ));
                    }
                }
            }
            CapacitySubject::AnonymousNetwork(_) | CapacitySubject::UnknownKeyed(_) => {
                return Err(WireError::new(
                    "resolution target cannot represent a shared provider key",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdapterResolutionEvidence {
    pub provider_key: ProviderKey,
    pub from_subject: CapacitySubject,
    pub to_subject: CapacitySubject,
    #[serde(deserialize_with = "required_option")]
    pub shared_account_key: Option<StableAccountKey>,
    #[serde(deserialize_with = "required_option")]
    pub shared_quota_scope_key: Option<StableQuotaScopeKey>,
    pub source: AdapterResolutionEvidenceSource,
    pub evidence_ref: EvidenceRef,
    pub observed_at: Timestamp,
    pub confidence: EvidenceConfidence,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalCredentialBinding {
    pub local_credential_ref: LocalCredentialRef,
    pub provider_key: ProviderKey,
    pub endpoint: EndpointKey,
    pub subject: CapacitySubject,
    pub first_seen_config_generation: ConfigGeneration,
    pub latest_seen_config_generation: ConfigGeneration,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestLogCursor {
    pub inode: FileInode,
    pub byte_offset: u64,
    pub generation: LogGeneration,
    pub dropped_records_total: u64,
    pub last_record_ts: Timestamp,
    pub trustworthy: bool,
    #[serde(deserialize_with = "required_option")]
    pub gap_reason: Option<LogGapReason>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogCursor {
    pub last_valid_generation: CatalogGeneration,
    pub last_valid_hash: Sha256,
    pub last_valid_at: Timestamp,
    pub last_attempt_at: Timestamp,
    pub consecutive_invalid: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeferredEvidenceRecordDescriptor {
    pub lane: EvidenceLaneKey,
    pub log_epoch: EvidenceLogEpoch,
    pub artifact_seq: u64,
    pub byte_start: u64,
    pub byte_len: u64,
    pub input_inventory_generation: InventoryGeneration,
    pub input_inventory_sha256: Sha256,
    pub record_sha256: Sha256,
    pub evidence_ref: EvidenceRef,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceLaneCursor {
    pub lane: EvidenceLaneKey,
    pub log_epoch: EvidenceLogEpoch,
    #[serde(deserialize_with = "required_option")]
    pub last_contiguous_artifact_seq: Option<u64>,
    #[serde(deserialize_with = "required_option")]
    pub last_contiguous_record_sha256: Option<Sha256>,
    pub next_scan_byte_offset: u64,
    pub quality: EvidenceLaneQuality,
    pub deferred_records: Vec<DeferredEvidenceRecordDescriptor>,
    pub updated_at: Timestamp,
}

impl WireValidate for EvidenceLaneCursor {
    fn validate(&self) -> Result<(), WireError> {
        if self.last_contiguous_artifact_seq.is_some()
            != self.last_contiguous_record_sha256.is_some()
        {
            return Err(WireError::new(
                "cursor contiguous sequence and hash must be present together",
            ));
        }
        if self.last_contiguous_artifact_seq.is_none() && !self.deferred_records.is_empty() {
            return Err(WireError::new(
                "deferred records cannot exist without a contiguous chain tip",
            ));
        }
        if self.quality == EvidenceLaneQuality::Valid && !self.deferred_records.is_empty() {
            return Err(WireError::new(
                "valid lane quality cannot retain deferred records",
            ));
        }
        if self.quality == EvidenceLaneQuality::DeferredUnknownInventoryGeneration
            && self.deferred_records.is_empty()
        {
            return Err(WireError::new(
                "deferred lane quality requires deferred records",
            ));
        }

        let lane_hash = lane_key_sha256(&self.lane)?;
        let expected_lane_kind = evidence_lane_kind(&self.lane);
        let mut previous_end = None;
        for (index, record) in self.deferred_records.iter().enumerate() {
            if record.artifact_seq == 0 {
                return Err(WireError::new("deferred artifact sequence is zero"));
            }
            if record.lane != self.lane {
                return Err(WireError::new(
                    "deferred record lane does not match owning cursor lane",
                ));
            }
            if index > 0 && record.artifact_seq != self.deferred_records[index - 1].artifact_seq + 1
            {
                return Err(WireError::new(
                    "deferred artifact sequences are not contiguous",
                ));
            }
            if let Some(tip) = self.last_contiguous_artifact_seq {
                if record.artifact_seq > tip {
                    return Err(WireError::new(
                        "deferred artifact sequence exceeds contiguous tip",
                    ));
                }
            }
            if record.log_epoch != self.log_epoch {
                return Err(WireError::new(
                    "deferred record epoch does not match cursor epoch",
                ));
            }
            if record.evidence_ref.sha256 != record.record_sha256 {
                return Err(WireError::new(
                    "deferred record evidence hash does not match record hash",
                ));
            }
            let parts = evidence_artifact_parts(&record.evidence_ref.uri_or_journal_key)
                .ok_or_else(|| WireError::new("deferred evidence ref is not an artifact key"))?;
            if parts[1] != expected_lane_kind || parts[2] != lane_hash {
                return Err(WireError::new(
                    "deferred evidence ref lane does not match cursor lane",
                ));
            }
            if parts[3].parse::<u64>().ok() != Some(record.log_epoch.get())
                || parts[4].parse::<u64>().ok() != Some(record.artifact_seq)
            {
                return Err(WireError::new(
                    "deferred evidence ref coordinates do not match descriptor",
                ));
            }
            if record.byte_len == 0 {
                return Err(WireError::new("deferred record byte_len is zero"));
            }
            let end = record
                .byte_start
                .checked_add(record.byte_len)
                .ok_or_else(|| WireError::new("deferred record byte range overflows"))?;
            if previous_end.is_some_and(|previous| record.byte_start < previous) {
                return Err(WireError::new("deferred record byte ranges overlap"));
            }
            previous_end = Some(end);
        }
        for pair in self.deferred_records.windows(2) {
            if pair[0].artifact_seq >= pair[1].artifact_seq {
                return Err(WireError::new("deferred records out of order"));
            }
        }
        if let Some(last) = self.deferred_records.last() {
            if self.last_contiguous_artifact_seq != Some(last.artifact_seq) {
                return Err(WireError::new(
                    "contiguous tip must equal greatest deferred artifact sequence",
                ));
            }
            if self.last_contiguous_record_sha256.as_ref() != Some(&last.record_sha256) {
                return Err(WireError::new(
                    "contiguous tip hash must equal greatest deferred record hash",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceLaneCursorEntry {
    pub lane: EvidenceLaneKey,
    pub cursor: EvidenceLaneCursor,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceCursors {
    #[serde(deserialize_with = "required_option")]
    pub request_log: Option<RequestLogCursor>,
    pub catalogs: BTreeMap<ProviderKey, CatalogCursor>,
    pub evidence_lanes: Vec<EvidenceLaneCursorEntry>,
}

impl WireValidate for SourceCursors {
    fn validate(&self) -> Result<(), WireError> {
        for entry in &self.evidence_lanes {
            if entry.lane != entry.cursor.lane {
                return Err(WireError::new("cursor lane does not match entry lane"));
            }
            entry.cursor.validate()?;
        }
        for pair in self.evidence_lanes.windows(2) {
            if canonical_cmp(&pair[0].lane, &pair[1].lane)? != Ordering::Less {
                return Err(WireError::new("evidence lanes out of order or duplicate"));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum EvidenceLaneKey {
    RegenerationImporter {
        logical_path: LogicalEvidencePath,
    },
    ServerDeployment {
        logical_path: LogicalEvidencePath,
        server_instance_id: ServerInstanceId,
    },
}

impl Serialize for EvidenceLaneKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        use serde::ser::SerializeMap;
        match self {
            Self::RegenerationImporter { logical_path } => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("kind", "regeneration_importer")?;
                map.serialize_entry("logical_path", logical_path)?;
                map.end()
            }
            Self::ServerDeployment {
                logical_path,
                server_instance_id,
            } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("kind", "server_deployment")?;
                map.serialize_entry("logical_path", logical_path)?;
                map.serialize_entry("server_instance_id", server_instance_id)?;
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for EvidenceLaneKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let object = value
            .as_object()
            .ok_or_else(|| serde::de::Error::custom("lane key must be an object"))?;
        let kind = object
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| serde::de::Error::custom("missing lane kind"))?;
        match kind {
            "regeneration_importer" => {
                if object.len() != 2 {
                    return Err(serde::de::Error::custom("wrong regeneration lane fields"));
                }
                Ok(Self::RegenerationImporter {
                    logical_path: serde_json::from_value(
                        object
                            .get("logical_path")
                            .cloned()
                            .ok_or_else(|| serde::de::Error::custom("missing logical_path"))?,
                    )
                    .map_err(serde::de::Error::custom)?,
                })
            }
            "server_deployment" => {
                if object.len() != 3 {
                    return Err(serde::de::Error::custom("wrong deployment lane fields"));
                }
                Ok(Self::ServerDeployment {
                    logical_path: serde_json::from_value(
                        object
                            .get("logical_path")
                            .cloned()
                            .ok_or_else(|| serde::de::Error::custom("missing logical_path"))?,
                    )
                    .map_err(serde::de::Error::custom)?,
                    server_instance_id: serde_json::from_value(
                        object.get("server_instance_id").cloned().ok_or_else(|| {
                            serde::de::Error::custom("missing server_instance_id")
                        })?,
                    )
                    .map_err(serde::de::Error::custom)?,
                })
            }
            _ => Err(serde::de::Error::custom("unknown lane kind")),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EvidenceLaneQuality {
    Valid,
    DeferredUnknownInventoryGeneration,
    Degraded { reason: EvidenceLaneDegradedReason },
}

impl Serialize for EvidenceLaneQuality {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        use serde::ser::SerializeMap;
        match self {
            Self::Valid => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("kind", "valid")?;
                map.end()
            }
            Self::DeferredUnknownInventoryGeneration => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("kind", "deferred_unknown_inventory_generation")?;
                map.end()
            }
            Self::Degraded { reason } => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("kind", "degraded")?;
                map.serialize_entry("reason", reason)?;
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for EvidenceLaneQuality {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let object = value
            .as_object()
            .ok_or_else(|| serde::de::Error::custom("lane quality must be an object"))?;
        let kind = object
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| serde::de::Error::custom("missing lane quality kind"))?;
        match (kind, object.len()) {
            ("valid", 1) => Ok(Self::Valid),
            ("deferred_unknown_inventory_generation", 1) => {
                Ok(Self::DeferredUnknownInventoryGeneration)
            }
            ("degraded", 2) => Ok(Self::Degraded {
                reason: serde_json::from_value(
                    object
                        .get("reason")
                        .cloned()
                        .ok_or_else(|| serde::de::Error::custom("missing degradation reason"))?,
                )
                .map_err(serde::de::Error::custom)?,
            }),
            _ => Err(serde::de::Error::custom("wrong lane quality shape")),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceLogReinitialization {
    pub lane: EvidenceLaneKey,
    pub new_epoch: EvidenceLogEpoch,
    pub prior_epoch: EvidenceLogEpoch,
    #[serde(deserialize_with = "required_option")]
    pub prior_terminal_artifact_seq: Option<u64>,
    #[serde(deserialize_with = "required_option")]
    pub prior_terminal_record_sha256: Option<Sha256>,
    pub reason: EvidenceLaneDegradedReason,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestLogPosition {
    pub log_generation: LogGeneration,
    pub inode: FileInode,
    pub byte_offset: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExactFailureObservation {
    pub observed_at: Timestamp,
    pub request_id: RequestId,
    pub evidence: EvidenceRef,
    #[serde(deserialize_with = "required_option")]
    pub source_position: Option<RequestLogPosition>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlidingEventLog {
    pub window: Duration,
    pub events: CanonicalMap<Timestamp, u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleEvidenceState {
    pub absence_run_count: u32,
    #[serde(deserialize_with = "required_option")]
    pub first_valid_absent_at: Option<Timestamp>,
    #[serde(deserialize_with = "required_option")]
    pub latest_valid_absent_at: Option<Timestamp>,
    #[serde(deserialize_with = "required_option")]
    pub latest_valid_absent_generation: Option<CatalogGeneration>,
    pub exact_notfound_24h: SlidingEventLog,
    #[serde(deserialize_with = "required_option")]
    pub latest_runtime_success_at: Option<Timestamp>,
    #[serde(deserialize_with = "required_option")]
    pub latest_zero_cost_success_at: Option<Timestamp>,
    #[serde(deserialize_with = "required_option")]
    pub latest_present_any_at: Option<Timestamp>,
    #[serde(deserialize_with = "required_option")]
    pub latest_present_any_generation: Option<CatalogGeneration>,
    #[serde(deserialize_with = "required_option")]
    pub latest_present_free_authoritative_at: Option<Timestamp>,
    #[serde(deserialize_with = "required_option")]
    pub latest_present_notfree_at: Option<Timestamp>,
    #[serde(deserialize_with = "required_option")]
    pub latest_present_free_unconfirmed_at: Option<Timestamp>,
    pub failed_recovery_attempts_7d: SlidingEventLog,
    #[serde(deserialize_with = "required_option")]
    pub latest_exact_notfound: Option<ExactFailureObservation>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CauseStateEntry {
    pub status: CauseStatus,
    pub activated_at: Timestamp,
    pub activation_evidence: EvidenceRef,
    #[serde(deserialize_with = "required_option")]
    pub clearing_started_at: Option<Timestamp>,
    #[serde(deserialize_with = "required_option")]
    pub clearing_evidence: Option<EvidenceRef>,
    #[serde(deserialize_with = "required_option")]
    pub last_interrupted_clearing: Option<InterruptedClearingAttempt>,
}

impl WireValidate for CauseStateEntry {
    fn validate(&self) -> Result<(), WireError> {
        if self.clearing_started_at.is_some() != self.clearing_evidence.is_some() {
            return Err(WireError::new(
                "clearing timestamp and evidence must be present together",
            ));
        }
        match self.status {
            CauseStatus::Active if self.clearing_started_at.is_some() => Err(WireError::new(
                "active cause cannot retain current clearing fields",
            )),
            CauseStatus::Clearing if self.clearing_started_at.is_none() => Err(WireError::new(
                "clearing cause requires current clearing fields",
            )),
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InterruptedClearingAttempt {
    pub started_at: Timestamp,
    pub evidence: EvidenceRef,
    pub interrupted_at: Timestamp,
    pub interrupting_evidence: EvidenceRef,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransientHold {
    pub kind: TransientHoldKind,
    pub subject: CapacitySubject,
    pub until: Timestamp,
    pub reason: EvidenceRef,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HoldState {
    pub active: CanonicalMap<TransientHoldKind, TransientHold>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryState {
    pub incident_id: RecoveryIncidentId,
    pub entered_at: Timestamp,
    pub entered_generation: InventoryGeneration,
    pub origin: RecoveryOrigin,
    pub confirmation: RecoveryConfirmation,
    pub regeneration: RecoveryRegeneration,
    pub flap: RecoveryFlapState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorCorrectionPayload {
    pub correction_id: OperatorCorrectionId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoveryOrigin {
    ExpiredCausesCleared,
    QuarantinedPositiveClearance,
    OperatorCorrection(OperatorCorrectionPayload),
}
tagged_enum_impl!(RecoveryOrigin,
    units {
        ExpiredCausesCleared => "expired_causes_cleared",
        QuarantinedPositiveClearance => "quarantined_positive_clearance",
    },
    payloads { OperatorCorrection(OperatorCorrectionPayload) => "operator_correction" }
);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryConfirmation {
    pub catalog_evidence_required: bool,
    pub authoritative_free_observations: CanonicalSet<Timestamp>,
    #[serde(deserialize_with = "required_option")]
    pub runtime_success_at: Option<Timestamp>,
    #[serde(deserialize_with = "required_option")]
    pub zero_cost_success_at: Option<Timestamp>,
    #[serde(deserialize_with = "required_option")]
    pub last_organic_admission_at: Option<Timestamp>,
    pub organic_admissions_24h: SlidingEventLog,
    #[serde(deserialize_with = "required_option")]
    pub confirmation_satisfied_at: Option<Timestamp>,
    #[serde(deserialize_with = "required_option")]
    pub confirmation_satisfied_by: Option<ConfirmationSatisfaction>,
}

impl WireValidate for RecoveryConfirmation {
    fn validate(&self) -> Result<(), WireError> {
        if self.confirmation_satisfied_at.is_some() != self.confirmation_satisfied_by.is_some() {
            return Err(WireError::new(
                "confirmation satisfaction timestamp and value must be present together",
            ));
        }
        Ok(())
    }
}

impl WireValidate for RecoveryState {
    fn validate(&self) -> Result<(), WireError> {
        self.confirmation.validate()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryRegeneration {
    pub inclusion_required: bool,
    pub never_excluded_since_incident: bool,
    #[serde(deserialize_with = "required_option")]
    pub latest_regeneration: Option<RegenerationFact>,
    #[serde(deserialize_with = "required_option")]
    pub latest_deployment: Option<DeploymentFact>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryFlapState {
    pub consecutive_failures: u32,
    #[serde(deserialize_with = "required_option")]
    pub damped_until: Option<Timestamp>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryEvaluation {
    pub evaluated_at: Timestamp,
    pub evaluated_generation: InventoryGeneration,
    pub confirmation_state: RecoveryConfirmationState,
    pub inclusion_state: RequirementState,
    pub loaded_deployment_state: RequirementState,
    pub ready_for_serving: bool,
    pub blocking_reasons: CanonicalSet<RecoveryBlocker>,
    pub required_evidence_inputs: Vec<EvidenceInputRequirementWire>,
    pub failed_attempts_7d_count: u32,
    pub evidence_refs: Vec<EvidenceRef>,
}

impl WireValidate for RecoveryEvaluation {
    fn validate(&self) -> Result<(), WireError> {
        if self.required_evidence_inputs.len() != 2
            || self.required_evidence_inputs[0].input != EvidenceInputKind::DeploymentEvidence
            || self.required_evidence_inputs[1].input != EvidenceInputKind::RegenerationEvidence
        {
            return Err(WireError::new(
                "required evidence inputs must be exactly deployment then regeneration",
            ));
        }
        for input in &self.required_evidence_inputs {
            if input.requirement == RequirementState::NotRequired
                && !input.required_lanes.values.is_empty()
            {
                return Err(WireError::new("not-required evidence input has lanes"));
            }
            let (expected, lane_matches) =
                match input.input {
                    EvidenceInputKind::DeploymentEvidence => {
                        (
                            self.loaded_deployment_state,
                            input.required_lanes.values.iter().all(|lane| {
                                matches!(lane, EvidenceLaneKey::ServerDeployment { .. })
                            }),
                        )
                    }
                    EvidenceInputKind::RegenerationEvidence => (
                        self.inclusion_state,
                        input.required_lanes.values.iter().all(|lane| {
                            matches!(lane, EvidenceLaneKey::RegenerationImporter { .. })
                        }),
                    ),
                };
            if input.requirement != expected {
                return Err(WireError::new(
                    "evidence input requirement does not match evaluation state",
                ));
            }
            if !lane_matches {
                return Err(WireError::new("evidence input lane has wrong lane kind"));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceInputRequirementWire {
    pub input: EvidenceInputKind,
    pub requirement: RequirementState,
    pub required_lanes: CanonicalSet<EvidenceLaneKey>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceInputRequirementSnapshot {
    pub requirement: RequirementState,
    pub required_lanes: CanonicalSet<EvidenceLaneKey>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SatisfiedPayload {
    pub satisfied_at: Timestamp,
    pub evidence: EvidenceRef,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoveryConfirmationState {
    Satisfied(SatisfiedPayload),
    PendingCatalogConfirmation,
    PendingOrganicConfirmation,
    NotRequiredBecauseNeverExcluded,
}
tagged_enum_impl!(RecoveryConfirmationState,
    units {
        PendingCatalogConfirmation => "pending_catalog_confirmation",
        PendingOrganicConfirmation => "pending_organic_confirmation",
        NotRequiredBecauseNeverExcluded => "not_required_because_never_excluded",
    },
    payloads { Satisfied(SatisfiedPayload) => "satisfied" }
);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimestampPayload {
    pub until: Timestamp,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceInputPayload {
    pub input: EvidenceInputKind,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RecoveryBlocker {
    ActiveCauseRemains,
    ClearingCauseRemains,
    ConfirmationMissing,
    RegenerationInclusionMissing,
    LoadedDeploymentMissing,
    StaleInventory,
    FlapDampedUntil(TimestampPayload),
    EvidenceInputDegraded(EvidenceInputPayload),
    EvidenceInputDeferred(EvidenceInputPayload),
}
tagged_enum_impl!(RecoveryBlocker,
    units {
        ActiveCauseRemains => "active_cause_remains",
        ClearingCauseRemains => "clearing_cause_remains",
        ConfirmationMissing => "confirmation_missing",
        RegenerationInclusionMissing => "regeneration_inclusion_missing",
        LoadedDeploymentMissing => "loaded_deployment_missing",
        StaleInventory => "stale_inventory",
    },
    payloads {
        FlapDampedUntil(TimestampPayload) => "flap_damped_until",
        EvidenceInputDegraded(EvidenceInputPayload) => "evidence_input_degraded",
        EvidenceInputDeferred(EvidenceInputPayload) => "evidence_input_deferred",
    }
);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorConfirmationAttestation {
    pub correction_id: OperatorCorrectionId,
    pub accepted_at: Timestamp,
    pub accepted_generation: InventoryGeneration,
    pub operator: OperatorIdentity,
    pub approval_ref: ApprovalRef,
    pub evidence_refs: Vec<EvidenceRef>,
    pub status: AttestationStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorAttestationRecord {
    pub attestation: OperatorConfirmationAttestation,
    #[serde(deserialize_with = "required_option")]
    pub incident_id: Option<RecoveryIncidentId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupersededPayload {
    pub at: Timestamp,
    pub evidence: EvidenceRef,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttestationStatus {
    Active,
    Superseded(SupersededPayload),
}
tagged_enum_impl!(AttestationStatus,
    units { Active => "active" },
    payloads { Superseded(SupersededPayload) => "superseded" }
);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ZeroCostRuntimePayload {
    pub observed_at: Timestamp,
    pub evidence: EvidenceRef,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogAuthoritativePayload {
    pub second_observed_at: Timestamp,
    pub evidence: EvidenceRef,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorAttestedPayload {
    pub correction_id: OperatorCorrectionId,
    pub accepted_at: Timestamp,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfirmationSatisfaction {
    ZeroCostRuntime(ZeroCostRuntimePayload),
    CatalogAuthoritative(CatalogAuthoritativePayload),
    OperatorAttested(OperatorAttestedPayload),
}
tagged_enum_impl!(ConfirmationSatisfaction,
    units {},
    payloads {
        ZeroCostRuntime(ZeroCostRuntimePayload) => "zero_cost_runtime",
        CatalogAuthoritative(CatalogAuthoritativePayload) => "catalog_authoritative",
        OperatorAttested(OperatorAttestedPayload) => "operator_attested",
    }
);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuspectPayload {
    pub sources: NonEmptyCanonicalSet<SuspectSource>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuarantinedPayload {
    pub sources: NonEmptyCanonicalSet<QuarantineSource>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpiredPayload {
    pub causes: NonEmptyCanonicalSet<ExpiryCause>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveringPayload {
    pub recovery: RecoveryState,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpiringSoonPayload {
    pub expires_at: Timestamp,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LifecycleState {
    Unknown,
    Serving,
    Suspect(SuspectPayload),
    Quarantined(QuarantinedPayload),
    Expired(ExpiredPayload),
    Recovering(Box<RecoveringPayload>),
    ExpiringSoon(ExpiringSoonPayload),
}
tagged_enum_impl!(LifecycleState,
    units {
        Unknown => "unknown",
        Serving => "serving",
    },
    payloads {
        Suspect(SuspectPayload) => "suspect",
        Quarantined(QuarantinedPayload) => "quarantined",
        Expired(ExpiredPayload) => "expired",
        Recovering(Box<RecoveringPayload>) => "recovering",
        ExpiringSoon(ExpiringSoonPayload) => "expiring_soon",
    }
);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryStateEntry {
    pub upstream_ref: UpstreamCapacityRef,
    pub lifecycle: LifecycleState,
    pub cause_states: CanonicalMap<ExpiryCause, CauseStateEntry>,
    pub evidence: LifecycleEvidenceState,
    pub state_since: Timestamp,
    pub last_transition_seq: u64,
    pub last_transition_generation: InventoryGeneration,
    #[serde(deserialize_with = "required_option")]
    pub recovery_evaluation: Option<RecoveryEvaluation>,
    pub adapter_confidence: EvidenceConfidence,
    pub operator_attestations: CanonicalMap<OperatorCorrectionId, OperatorAttestationRecord>,
    pub holds: HoldState,
}

impl WireValidate for InventoryStateEntry {
    fn validate(&self) -> Result<(), WireError> {
        if let Some(evaluation) = &self.recovery_evaluation {
            evaluation.validate()?;
        }
        if let LifecycleState::Recovering(payload) = &self.lifecycle {
            payload.recovery.validate()?;
        }
        for cause in &self.cause_states.entries {
            cause.value.validate()?;
        }
        for entry in &self.holds.active.entries {
            if entry.key != entry.value.kind {
                return Err(WireError::new(
                    "persistent hold key does not match hold kind",
                ));
            }
            if entry.value.subject != self.upstream_ref.identity.subject {
                return Err(WireError::new(
                    "persistent hold subject does not match state entry",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedInventory {
    pub schema_version: u64,
    pub writer: String,
    pub writer_instance: WriterInstanceId,
    pub generation: InventoryGeneration,
    pub previous_generation: InventoryGeneration,
    #[serde(deserialize_with = "required_option")]
    pub previous_sha256: Option<Sha256>,
    pub generated_at: Timestamp,
    pub heartbeat_at: Timestamp,
    pub source_cursors: SourceCursors,
    pub states: CanonicalMap<UpstreamCapacityRef, InventoryStateEntry>,
    pub bindings: CanonicalMap<LocalCredentialRef, LocalCredentialBinding>,
    pub scope_resolutions: Vec<ScopeResolution>,
    pub transition_watermark: u64,
    pub signature: InventorySignature,
}

impl WireValidate for SignedInventory {
    fn validate(&self) -> Result<(), WireError> {
        if self.schema_version != 10 {
            return Err(WireError::new("inventory schema version must be 10"));
        }
        if self.writer != "llmux2-health-reconcile" {
            return Err(WireError::new("wrong inventory writer"));
        }
        self.source_cursors.validate()?;
        for state in &self.states.entries {
            if state.key != state.value.upstream_ref {
                return Err(WireError::new("state key does not match upstream_ref"));
            }
            state.value.validate()?;
        }
        for binding in &self.bindings.entries {
            if binding.key != binding.value.local_credential_ref {
                return Err(WireError::new(
                    "binding key does not match local credential ref",
                ));
            }
        }
        for resolution in &self.scope_resolutions {
            resolution.validate()?;
        }
        for (index, left) in self.scope_resolutions.iter().enumerate() {
            for right in &self.scope_resolutions[index + 1..] {
                if left.from == right.to && left.to == right.from {
                    return Err(WireError::new("reverse scope resolution edge"));
                }
                if left.from == right.from && left.to != right.to {
                    return Err(WireError::new(
                        "one scope-resolution source has multiple targets",
                    ));
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegenerationEvidenceRow {
    pub inclusion: RegenerationInclusionOutcome,
    #[serde(deserialize_with = "required_option")]
    pub expected_deployment_fingerprint: Option<DeploymentFingerprint>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerDeploymentEvidenceRow {
    pub present: bool,
    #[serde(deserialize_with = "required_option")]
    pub actual_deployment_fingerprint: Option<DeploymentFingerprint>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceRowEntry<R> {
    pub upstream_ref: UpstreamCapacityRef,
    pub row: R,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegenerationEvidenceRecord {
    pub schema_version: u64,
    pub artifact_seq: u64,
    pub log_epoch: EvidenceLogEpoch,
    #[serde(deserialize_with = "required_option")]
    pub previous_record_sha256: Option<Sha256>,
    pub record_sha256: Sha256,
    #[serde(deserialize_with = "required_option")]
    pub continuity_reinitialization: Option<EvidenceLogReinitialization>,
    pub import_run_id: ImportRunId,
    pub started_at: Timestamp,
    pub finished_at: Timestamp,
    pub input_inventory_generation: InventoryGeneration,
    pub input_inventory_sha256: Sha256,
    #[serde(deserialize_with = "required_option")]
    pub config_generation: Option<ConfigGeneration>,
    #[serde(deserialize_with = "required_option")]
    pub config_sha256: Option<Sha256>,
    pub fingerprint_algorithm: DeploymentFingerprintAlgorithm,
    pub status: RegenerationEvidenceStatus,
    #[serde(deserialize_with = "required_option")]
    pub failure_coverage: Option<RegenerationFailureCoverage>,
    pub rows: Vec<EvidenceRowEntry<RegenerationEvidenceRow>>,
}

fn validate_record_self_hash<T: Serialize>(record: &T, actual: &Sha256) -> Result<(), WireError> {
    if evidence_record_sha256(record)? != actual.as_str() {
        return Err(WireError::new(
            "record hash does not match canonical hash input",
        ));
    }
    Ok(())
}

fn validate_artifact_checkpoint(
    artifact_seq: u64,
    log_epoch: EvidenceLogEpoch,
    checkpoint: &Option<EvidenceLogReinitialization>,
) -> Result<(), WireError> {
    if artifact_seq == 0 || log_epoch.get() == 0 {
        return Err(WireError::new("artifact sequence and epoch start at one"));
    }
    match (artifact_seq, log_epoch.get(), checkpoint) {
        (1, 1, None) => Ok(()),
        (1, 1, Some(_)) => Err(WireError::new(
            "initial lane epoch cannot carry a reinitialization checkpoint",
        )),
        (1, _, None) => Err(WireError::new(
            "first artifact of a later epoch is missing its checkpoint",
        )),
        (1, epoch, Some(checkpoint)) => {
            if checkpoint.new_epoch.get() != epoch {
                return Err(WireError::new(
                    "checkpoint new epoch does not match artifact epoch",
                ));
            }
            if checkpoint.prior_epoch.get().checked_add(1) != Some(epoch) {
                return Err(WireError::new(
                    "checkpoint prior epoch is not the immediately previous epoch",
                ));
            }
            if checkpoint.prior_terminal_artifact_seq.is_some()
                != checkpoint.prior_terminal_record_sha256.is_some()
            {
                return Err(WireError::new(
                    "checkpoint terminal sequence and hash must be present together",
                ));
            }
            Ok(())
        }
        (_, _, Some(_)) => Err(WireError::new(
            "non-initial artifact cannot carry a reinitialization checkpoint",
        )),
        (_, _, None) => Ok(()),
    }
}

impl WireValidate for RegenerationEvidenceRecord {
    fn validate(&self) -> Result<(), WireError> {
        if self.schema_version != 3 {
            return Err(WireError::new("evidence schema version must be 3"));
        }
        validate_record_self_hash(self, &self.record_sha256)?;
        validate_artifact_checkpoint(
            self.artifact_seq,
            self.log_epoch,
            &self.continuity_reinitialization,
        )?;
        if let Some(checkpoint) = &self.continuity_reinitialization {
            if !matches!(
                checkpoint.lane,
                EvidenceLaneKey::RegenerationImporter { .. }
            ) {
                return Err(WireError::new(
                    "regeneration checkpoint must name a regeneration lane",
                ));
            }
        }
        validate_rows(&self.rows)?;
        for row in &self.rows {
            validate_regeneration_row_optionality(&row.row)?;
        }
        if (self.artifact_seq == 1) == self.previous_record_sha256.is_some() {
            return Err(WireError::new("wrong previous record hash presence"));
        }
        match (&self.status, &self.failure_coverage) {
            (RegenerationEvidenceStatus::Success, None) => {
                if self.config_generation.is_none() || self.config_sha256.is_none() {
                    return Err(WireError::new(
                        "successful regeneration lacks config coordinates",
                    ));
                }
                if self.rows.iter().any(|entry| {
                    matches!(
                        entry.row.inclusion,
                        RegenerationInclusionOutcome::ImportFailed(_)
                    )
                }) {
                    return Err(WireError::new(
                        "success artifact cannot carry import-failed rows",
                    ));
                }
            }
            (RegenerationEvidenceStatus::Failed(failure), Some(coverage)) => match coverage {
                RegenerationFailureCoverage::UnavailableBeforeInventoryEnumeration => {
                    if !self.rows.is_empty() {
                        return Err(WireError::new(
                            "unavailable-before-enumeration failure coverage requires empty rows",
                        ));
                    }
                }
                RegenerationFailureCoverage::CompleteInputLifecycleUniverse => {
                    for entry in &self.rows {
                        match &entry.row.inclusion {
                                RegenerationInclusionOutcome::ImportFailed(row_failure)
                                    if row_failure.kind == failure.kind => {}
                                _ => {
                                    return Err(WireError::new(
                                        "complete failure coverage rows must carry the same import failure kind",
                                    ))
                                }
                            }
                    }
                }
            },
            _ => {
                return Err(WireError::new(
                    "failure coverage does not match regeneration status",
                ))
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerDeploymentEvidenceRecord {
    pub schema_version: u64,
    pub artifact_seq: u64,
    pub log_epoch: EvidenceLogEpoch,
    #[serde(deserialize_with = "required_option")]
    pub previous_record_sha256: Option<Sha256>,
    pub record_sha256: Sha256,
    #[serde(deserialize_with = "required_option")]
    pub continuity_reinitialization: Option<EvidenceLogReinitialization>,
    pub server_instance_id: ServerInstanceId,
    pub boot_id: BootId,
    pub record_kind: DeploymentEvidenceRecordKind,
    pub emitted_at: Timestamp,
    pub load_started_at: Timestamp,
    pub load_finished_at: Timestamp,
    pub input_inventory_generation: InventoryGeneration,
    pub input_inventory_sha256: Sha256,
    pub loaded_config_generation: ConfigGeneration,
    pub loaded_config_sha256: Sha256,
    pub fingerprint_algorithm: DeploymentFingerprintAlgorithm,
    pub traffic_role: DeploymentTrafficRole,
    pub rows: Vec<EvidenceRowEntry<ServerDeploymentEvidenceRow>>,
}

impl WireValidate for ServerDeploymentEvidenceRecord {
    fn validate(&self) -> Result<(), WireError> {
        if self.schema_version != 3 {
            return Err(WireError::new("evidence schema version must be 3"));
        }
        validate_record_self_hash(self, &self.record_sha256)?;
        validate_artifact_checkpoint(
            self.artifact_seq,
            self.log_epoch,
            &self.continuity_reinitialization,
        )?;
        if let Some(checkpoint) = &self.continuity_reinitialization {
            match &checkpoint.lane {
                EvidenceLaneKey::ServerDeployment {
                    server_instance_id, ..
                } if server_instance_id == &self.server_instance_id => {}
                _ => {
                    return Err(WireError::new(
                        "deployment checkpoint lane does not match record server instance",
                    ))
                }
            }
        }
        validate_rows(&self.rows)?;
        if (self.artifact_seq == 1) == self.previous_record_sha256.is_some() {
            return Err(WireError::new("wrong previous record hash presence"));
        }
        for row in &self.rows {
            validate_deployment_row_optionality(&row.row)?;
        }
        Ok(())
    }
}

fn validate_regeneration_row_optionality(row: &RegenerationEvidenceRow) -> Result<(), WireError> {
    let must_have = matches!(row.inclusion, RegenerationInclusionOutcome::Included);
    if row.expected_deployment_fingerprint.is_some() != must_have {
        return Err(WireError::new("wrong expected fingerprint presence"));
    }
    Ok(())
}

fn validate_deployment_row_optionality(row: &ServerDeploymentEvidenceRow) -> Result<(), WireError> {
    if row.actual_deployment_fingerprint.is_some() != row.present {
        return Err(WireError::new("wrong actual fingerprint presence"));
    }
    Ok(())
}

fn validate_rows<R>(rows: &[EvidenceRowEntry<R>]) -> Result<(), WireError> {
    for pair in rows.windows(2) {
        if canonical_cmp(&pair[0].upstream_ref, &pair[1].upstream_ref)? != Ordering::Less {
            return Err(WireError::new("evidence rows out of order or duplicate"));
        }
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegenerationFailurePayload {
    pub kind: RegenerationFailureKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegenerationEvidenceStatus {
    Success,
    Failed(RegenerationFailurePayload),
}
tagged_enum_impl!(RegenerationEvidenceStatus,
    units { Success => "success" },
    payloads { Failed(RegenerationFailurePayload) => "failed" }
);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExclusionReasonsPayload {
    pub reasons: NonEmptyCanonicalSet<RegenerationExclusionReason>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegenerationInclusionOutcome {
    Included,
    Excluded(ExclusionReasonsPayload),
    ImportFailed(RegenerationFailurePayload),
}
tagged_enum_impl!(RegenerationInclusionOutcome,
    units { Included => "included" },
    payloads {
        Excluded(ExclusionReasonsPayload) => "excluded",
        ImportFailed(RegenerationFailurePayload) => "import_failed",
    }
);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IncludedRegenerationPayload {
    pub config_generation: ConfigGeneration,
    pub config_sha256: Sha256,
    pub expected_deployment_fingerprint: DeploymentFingerprint,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObservedRegenerationOutcome {
    Included(IncludedRegenerationPayload),
    Excluded(ExclusionReasonsPayload),
    ImportFailed(RegenerationFailurePayload),
}
tagged_enum_impl!(ObservedRegenerationOutcome,
    units {},
    payloads {
        Included(IncludedRegenerationPayload) => "included",
        Excluded(ExclusionReasonsPayload) => "excluded",
        ImportFailed(RegenerationFailurePayload) => "import_failed",
    }
);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresentDeploymentPayload {
    pub loaded_config_generation: ConfigGeneration,
    pub loaded_config_sha256: Sha256,
    pub actual_deployment_fingerprint: DeploymentFingerprint,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AbsentDeploymentPayload {
    pub loaded_config_generation: ConfigGeneration,
    pub loaded_config_sha256: Sha256,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObservedDeploymentOutcome {
    Present(PresentDeploymentPayload),
    Absent(AbsentDeploymentPayload),
}
tagged_enum_impl!(ObservedDeploymentOutcome,
    units {},
    payloads {
        Present(PresentDeploymentPayload) => "present",
        Absent(AbsentDeploymentPayload) => "absent",
    }
);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegenerationEvidenceObserved {
    pub upstream_ref: UpstreamCapacityRef,
    pub lane: EvidenceLaneKey,
    pub log_epoch: EvidenceLogEpoch,
    pub artifact_seq: u64,
    pub import_run_id: ImportRunId,
    pub input_inventory_generation: InventoryGeneration,
    pub input_inventory_sha256: Sha256,
    pub fingerprint_algorithm: DeploymentFingerprintAlgorithm,
    pub observed_at: Timestamp,
    pub outcome: ObservedRegenerationOutcome,
    pub evidence_ref: EvidenceRef,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerDeploymentEvidenceObserved {
    pub upstream_ref: UpstreamCapacityRef,
    pub lane: EvidenceLaneKey,
    pub artifact_seq: u64,
    pub log_epoch: EvidenceLogEpoch,
    pub server_instance_id: ServerInstanceId,
    pub boot_id: BootId,
    pub traffic_role: DeploymentTrafficRole,
    pub input_inventory_generation: InventoryGeneration,
    pub input_inventory_sha256: Sha256,
    pub fingerprint_algorithm: DeploymentFingerprintAlgorithm,
    pub observed_at: Timestamp,
    pub outcome: ObservedDeploymentOutcome,
    pub evidence_ref: EvidenceRef,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegenerationFact {
    pub lane: EvidenceLaneKey,
    pub log_epoch: EvidenceLogEpoch,
    pub artifact_seq: u64,
    pub input_inventory_generation: InventoryGeneration,
    pub input_inventory_sha256: Sha256,
    pub fingerprint_algorithm: DeploymentFingerprintAlgorithm,
    pub outcome: ObservedRegenerationOutcome,
    pub observed_at: Timestamp,
    pub evidence_ref: EvidenceRef,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentFact {
    pub lane: EvidenceLaneKey,
    pub artifact_seq: u64,
    pub log_epoch: EvidenceLogEpoch,
    pub server_instance_id: ServerInstanceId,
    pub boot_id: BootId,
    pub input_inventory_generation: InventoryGeneration,
    pub input_inventory_sha256: Sha256,
    pub fingerprint_algorithm: DeploymentFingerprintAlgorithm,
    pub outcome: ObservedDeploymentOutcome,
    pub observed_at: Timestamp,
    pub evidence_ref: EvidenceRef,
}

fn parse_wire<T>(input: &str, surface: &str) -> Result<T, WireError>
where
    T: DeserializeOwned + WireValidate,
{
    reject_duplicate_object_members(input)?;
    let value: Value = serde_json::from_str(input)
        .map_err(|error| WireError::new(format!("invalid JSON: {error}")))?;
    validate_json_numbers(&value)?;
    let parsed: T = serde_json::from_value(value)
        .map_err(|error| WireError::new(format!("invalid {surface}: {error}")))?;
    parsed.validate()?;
    Ok(parsed)
}

pub fn parse_signed_inventory_pr1(input: &str) -> Result<SignedInventory, WireError> {
    parse_wire(input, "signed inventory PR1 projection")
}

pub fn parse_regeneration_evidence_record(
    input: &str,
) -> Result<RegenerationEvidenceRecord, WireError> {
    parse_wire(input, "regeneration evidence record")
}

pub fn parse_server_deployment_evidence_record(
    input: &str,
) -> Result<ServerDeploymentEvidenceRecord, WireError> {
    parse_wire(input, "server deployment evidence record")
}

fn validate_json_numbers(value: &Value) -> Result<(), WireError> {
    match value {
        Value::Number(number) => {
            if number.as_u64().is_some_and(|n| n <= MAX_SAFE_INTEGER) {
                Ok(())
            } else {
                Err(WireError::new("noncanonical integer"))
            }
        }
        Value::Array(values) => values.iter().try_for_each(validate_json_numbers),
        Value::Object(object) => object.values().try_for_each(validate_json_numbers),
        _ => Ok(()),
    }
}

fn reject_duplicate_object_members(input: &str) -> Result<(), WireError> {
    struct Scanner<'a> {
        bytes: &'a [u8],
        input: &'a str,
        pos: usize,
    }
    impl<'a> Scanner<'a> {
        fn ws(&mut self) {
            while self
                .bytes
                .get(self.pos)
                .is_some_and(u8::is_ascii_whitespace)
            {
                self.pos += 1;
            }
        }
        fn expect(&mut self, byte: u8) -> Result<(), WireError> {
            self.ws();
            if self.bytes.get(self.pos) == Some(&byte) {
                self.pos += 1;
                Ok(())
            } else {
                Err(WireError::new("invalid JSON punctuation"))
            }
        }
        fn string_token(&mut self) -> Result<&'a str, WireError> {
            self.ws();
            if self.bytes.get(self.pos) != Some(&b'"') {
                return Err(WireError::new("expected JSON string"));
            }
            let start = self.pos;
            self.pos += 1;
            while let Some(&byte) = self.bytes.get(self.pos) {
                match byte {
                    b'"' => {
                        self.pos += 1;
                        return Ok(&self.input[start..self.pos]);
                    }
                    b'\\' => {
                        self.pos += 1;
                        if self.bytes.get(self.pos).is_none() {
                            return Err(WireError::new("unterminated escape"));
                        }
                        self.pos += 1;
                    }
                    0x00..=0x1f => return Err(WireError::new("control byte in string")),
                    _ => self.pos += 1,
                }
            }
            Err(WireError::new("unterminated string"))
        }
        fn value(&mut self) -> Result<(), WireError> {
            self.ws();
            match self.bytes.get(self.pos).copied() {
                Some(b'{') => self.object(),
                Some(b'[') => self.array(),
                Some(b'"') => self.string_token().map(|_| ()),
                Some(b't') => self.literal("true"),
                Some(b'f') => self.literal("false"),
                Some(b'n') => self.literal("null"),
                Some(b'-' | b'0'..=b'9') => self.number(),
                _ => Err(WireError::new("invalid JSON value")),
            }
        }
        fn literal(&mut self, literal: &str) -> Result<(), WireError> {
            if self.input[self.pos..].starts_with(literal) {
                self.pos += literal.len();
                Ok(())
            } else {
                Err(WireError::new("invalid JSON literal"))
            }
        }
        fn number(&mut self) -> Result<(), WireError> {
            while self.bytes.get(self.pos).is_some_and(|b| {
                b.is_ascii_digit() || matches!(b, b'-' | b'+' | b'.' | b'e' | b'E')
            }) {
                self.pos += 1;
            }
            Ok(())
        }
        fn array(&mut self) -> Result<(), WireError> {
            self.expect(b'[')?;
            self.ws();
            if self.bytes.get(self.pos) == Some(&b']') {
                self.pos += 1;
                return Ok(());
            }
            loop {
                self.value()?;
                self.ws();
                match self.bytes.get(self.pos).copied() {
                    Some(b',') => self.pos += 1,
                    Some(b']') => {
                        self.pos += 1;
                        return Ok(());
                    }
                    _ => return Err(WireError::new("invalid array")),
                }
            }
        }
        fn object(&mut self) -> Result<(), WireError> {
            self.expect(b'{')?;
            let mut keys = std::collections::HashSet::new();
            self.ws();
            if self.bytes.get(self.pos) == Some(&b'}') {
                self.pos += 1;
                return Ok(());
            }
            loop {
                let key = self.string_token()?;
                let decoded_key: String = serde_json::from_str(key)
                    .map_err(|_| WireError::new("invalid object key string"))?;
                if !keys.insert(decoded_key) {
                    return Err(WireError::new("duplicate decoded object member"));
                }
                self.expect(b':')?;
                self.value()?;
                self.ws();
                match self.bytes.get(self.pos).copied() {
                    Some(b',') => self.pos += 1,
                    Some(b'}') => {
                        self.pos += 1;
                        return Ok(());
                    }
                    _ => return Err(WireError::new("invalid object")),
                }
            }
        }
    }
    let mut scanner = Scanner {
        bytes: input.as_bytes(),
        input,
        pos: 0,
    };
    scanner.value()?;
    scanner.ws();
    if scanner.pos == input.len() {
        Ok(())
    } else {
        Err(WireError::new("trailing JSON input"))
    }
}

pub fn to_canonical_json<T: Serialize + ?Sized>(value: &T) -> Result<String, WireError> {
    let value = serde_json::to_value(value).map_err(|error| WireError::new(error.to_string()))?;
    validate_json_numbers(&value)?;
    let mut output = String::new();
    write_jcs(&value, &mut output)?;
    Ok(output)
}

fn utf16_cmp(left: &str, right: &str) -> Ordering {
    left.encode_utf16().cmp(right.encode_utf16())
}

fn write_jcs(value: &Value, output: &mut String) -> Result<(), WireError> {
    match value {
        Value::Null => output.push_str("null"),
        Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
        Value::Number(number) => output.push_str(
            &number
                .as_u64()
                .ok_or_else(|| WireError::new("noncanonical integer"))?
                .to_string(),
        ),
        Value::String(value) => write_json_string(value, output),
        Value::Array(values) => {
            output.push('[');
            for (index, item) in values.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                write_jcs(item, output)?;
            }
            output.push(']');
        }
        Value::Object(object) => {
            output.push('{');
            let mut keys: Vec<_> = object.keys().collect();
            keys.sort_by(|left, right| utf16_cmp(left, right));
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                write_json_string(key, output);
                output.push(':');
                write_jcs(&object[*key], output)?;
            }
            output.push('}');
        }
    }
    Ok(())
}

fn write_json_string(value: &str, output: &mut String) {
    output.push('"');
    for ch in value.chars() {
        match ch {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\u{08}' => output.push_str("\\b"),
            '\t' => output.push_str("\\t"),
            '\n' => output.push_str("\\n"),
            '\u{0c}' => output.push_str("\\f"),
            '\r' => output.push_str("\\r"),
            c if c <= '\u{1f}' => output.push_str(&format!("\\u{:04x}", c as u32)),
            c => output.push(c),
        }
    }
    output.push('"');
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256Digest::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn canonical_without_top_level<T: Serialize + ?Sized>(
    value: &T,
    remove: &str,
) -> Result<Vec<u8>, WireError> {
    let mut value =
        serde_json::to_value(value).map_err(|error| WireError::new(error.to_string()))?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| WireError::new("hash input is not an object"))?;
    object.remove(remove);
    Ok(to_canonical_json(&value)?.into_bytes())
}

pub fn evidence_record_sha256<T: Serialize + ?Sized>(record: &T) -> Result<String, WireError> {
    Ok(sha256_hex(&canonical_without_top_level(
        record,
        "record_sha256",
    )?))
}

pub fn lane_key_sha256(lane: &EvidenceLaneKey) -> Result<String, WireError> {
    Ok(sha256_hex(&canonical_bytes(lane)?))
}

pub fn inventory_sha256_without_signature(
    inventory: &SignedInventory,
) -> Result<String, WireError> {
    Ok(sha256_hex(&canonical_without_top_level(
        inventory,
        "signature",
    )?))
}

pub fn inventory_signature_payload(inventory: &SignedInventory) -> Result<Vec<u8>, WireError> {
    let mut payload = SIGNATURE_DOMAIN.to_vec();
    payload.extend(canonical_without_top_level(inventory, "signature")?);
    Ok(payload)
}

pub fn render_wire_type_inventory() -> String {
    let mut output = String::from("type\tkind\tfields\n");
    for row in wire_type_inventory() {
        output.push_str(row.type_name);
        output.push('\t');
        output.push_str(row.kind);
        if !row.fields.is_empty() {
            output.push('\t');
            output.push_str(&row.fields.join(","));
        }
        output.push('\n');
    }
    output
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WireTypeRow {
    pub type_name: &'static str,
    pub kind: &'static str,
    pub fields: Vec<&'static str>,
}

pub fn wire_type_inventory() -> Vec<WireTypeRow> {
    static INVENTORY: std::sync::OnceLock<Vec<WireTypeRow>> = std::sync::OnceLock::new();
    INVENTORY
        .get_or_init(|| derive_inventory_from_source(include_str!("upstream_expiry.rs")))
        .clone()
}

fn derive_inventory_from_source(source: &str) -> Vec<WireTypeRow> {
    let mut rows = Vec::new();
    let bytes = source.as_bytes();
    let mut pos = 0;
    while pos < source.len() {
        let struct_at = source[pos..].find("pub struct ").map(|i| pos + i);
        let enum_at = source[pos..].find("pub enum ").map(|i| pos + i);
        let (at, kind) = match (struct_at, enum_at) {
            (Some(s), Some(e)) if s < e => (s, "struct"),
            (Some(_s), Some(e)) => (e, "enum"),
            (Some(s), None) => (s, "struct"),
            (None, Some(e)) => (e, "enum"),
            (None, None) => break,
        };
        let prefix = if kind == "struct" {
            "pub struct "
        } else {
            "pub enum "
        };
        let name_start = at + prefix.len();
        let name_end = source[name_start..]
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .map(|i| name_start + i)
            .unwrap_or(source.len());
        let name = &source[name_start..name_end];
        let mut fields = Vec::new();
        if kind == "struct" {
            if let Some(open_rel) = source[name_end..].find('{') {
                let open = name_end + open_rel;
                let mut depth = 0usize;
                let mut close = open;
                for (index, byte) in bytes[open..].iter().enumerate() {
                    match byte {
                        b'{' => depth += 1,
                        b'}' => {
                            depth -= 1;
                            if depth == 0 {
                                close = open + index;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                let body = &source[open + 1..close];
                for line in body.lines() {
                    let trimmed = line.trim();
                    if let Some(rest) = trimmed.strip_prefix("pub ") {
                        if let Some(field) = rest.split(':').next() {
                            let field = field.trim();
                            if !field.is_empty()
                                && field.bytes().all(|b| {
                                    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'
                                })
                            {
                                fields.push(field);
                            }
                        }
                    }
                }
            }
        }
        if !name.is_empty() && !matches!(name, "WireError" | "WireTypeRow") {
            let kind = if name == "Https" { "brand" } else { kind };
            rows.push(WireTypeRow {
                type_name: leak_static(name),
                kind,
                fields: fields.into_iter().map(leak_static).collect(),
            });
        }
        pos = name_end;
    }
    for line in source.lines() {
        let trimmed = line.trim();
        for (prefix, kind) in [
            ("unit_enum!(", "enum"),
            ("string_brand!(", "brand"),
            ("integer_brand!(", "brand"),
        ] {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                if !name.is_empty() {
                    rows.push(WireTypeRow {
                        type_name: leak_static(&name),
                        kind,
                        fields: Vec::new(),
                    });
                }
            }
        }
    }
    rows.sort_by(|left, right| left.type_name.cmp(right.type_name));
    rows.dedup_by(|left, right| left.type_name == right.type_name);
    rows
}

fn leak_static<T: AsRef<str> + ?Sized>(value: &T) -> &'static str {
    Box::leak(value.as_ref().to_owned().into_boxed_str())
}
