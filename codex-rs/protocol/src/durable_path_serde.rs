use codex_utils_path_uri::LegacyAppPathString;
use codex_utils_path_uri::PathUri;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::Serializer;
use serde::de::DeserializeOwned;
use serde::de::Error as _;
use serde_json::Value;

use crate::models::PermissionProfile;
use crate::protocol::SandboxPolicy;

#[derive(Clone, Debug, PartialEq, Eq)]
enum DurablePathValueInner<T> {
    Native(T),
    Foreign(Value),
}

/// A durable value that preserves path spelling from a foreign operating system.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DurablePathValue<T>(DurablePathValueInner<T>);

impl<T> DurablePathValue<T>
where
    T: Clone + DeserializeOwned,
{
    pub fn to_native(&self) -> Result<T, serde_json::Error> {
        match &self.0 {
            DurablePathValueInner::Native(value) => Ok(value.clone()),
            DurablePathValueInner::Foreign(_) => {
                Err(serde_json::Error::custom("value contains foreign paths"))
            }
        }
    }
}

impl<T> From<T> for DurablePathValue<T> {
    fn from(value: T) -> Self {
        Self(DurablePathValueInner::Native(value))
    }
}

impl<T> Serialize for DurablePathValue<T>
where
    T: Serialize,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match &self.0 {
            DurablePathValueInner::Native(value) => value.serialize(serializer),
            DurablePathValueInner::Foreign(value) => value.serialize(serializer),
        }
    }
}

impl<'de, T> Deserialize<'de> for DurablePathValue<T>
where
    T: DeserializeOwned,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        if contains_foreign_native_path(&value) {
            return Ok(Self(DurablePathValueInner::Foreign(value)));
        }
        serde_json::from_value(value)
            .map(DurablePathValueInner::Native)
            .map(Self)
            .map_err(D::Error::custom)
    }
}

/// A permission profile whose serialized paths retain their originating host convention.
pub type DurablePermissionProfile = DurablePathValue<PermissionProfile>;

/// A legacy sandbox policy whose serialized paths retain their originating host convention.
pub type DurableSandboxPolicy = DurablePathValue<SandboxPolicy>;

fn contains_foreign_native_path(value: &Value) -> bool {
    match value {
        Value::Array(values) => values.iter().any(contains_foreign_native_path),
        Value::Object(fields) => fields.values().any(contains_foreign_native_path),
        Value::String(value) => is_foreign_native_path(value),
        Value::Null | Value::Bool(_) | Value::Number(_) => false,
    }
}

fn is_foreign_native_path(path: &str) -> bool {
    let path = if path.starts_with("file:") {
        PathUri::parse(path).map_err(|err| err.to_string())
    } else {
        serde_json::from_value::<LegacyAppPathString>(Value::String(path.to_string()))
            .map_err(|err| err.to_string())
            .and_then(|path| PathUri::try_from(path).map_err(|err| err.to_string()))
    };
    path.is_ok_and(|path| path.to_abs_path().is_err())
}

/// Serde bridge that retains the historical native-path string on the wire.
pub mod legacy_native_path_uri {
    use super::*;

    pub fn serialize<S>(path: &PathUri, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        LegacyAppPathString::from(path.clone()).serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<PathUri, D::Error>
    where
        D: Deserializer<'de>,
    {
        let path = LegacyAppPathString::deserialize(deserializer)?;
        parse(path).map_err(D::Error::custom)
    }

    fn parse(path: LegacyAppPathString) -> Result<PathUri, String> {
        if path.as_str().starts_with("file:") {
            return PathUri::parse(path.as_str()).map_err(|err| err.to_string());
        }
        PathUri::try_from(path).map_err(|err| err.to_string())
    }

    pub mod option_vec {
        use super::*;

        pub fn serialize<S>(paths: &Option<Vec<PathUri>>, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            paths
                .as_ref()
                .map(|paths| {
                    paths
                        .iter()
                        .cloned()
                        .map(LegacyAppPathString::from)
                        .collect::<Vec<_>>()
                })
                .serialize(serializer)
        }

        pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Vec<PathUri>>, D::Error>
        where
            D: Deserializer<'de>,
        {
            Option::<Vec<LegacyAppPathString>>::deserialize(deserializer)?
                .map(|paths| {
                    paths
                        .into_iter()
                        .map(super::parse)
                        .collect::<Result<Vec<_>, _>>()
                })
                .transpose()
                .map_err(D::Error::custom)
        }
    }
}
