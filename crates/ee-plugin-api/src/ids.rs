//! Stable identifier newtypes shared by every plugin contract.
//!
//! Identifiers are the manifest-visible names used by diagnostics, dependency
//! resolution, contributions, and the external wire protocol. They validate on
//! construction and on deserialization so an invalid id can never reach a host
//! registry.

use std::fmt;
use std::str::FromStr;

use schemars::JsonSchema;
use serde::de::{self, Deserialize, Deserializer, Visitor};
use serde::ser::{Serialize, Serializer};

/// Maximum accepted identifier length in bytes.
pub const MAX_ID_LEN: usize = 256;

/// Errors produced when an identifier is malformed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    #[error("identifier must not be empty")]
    Empty,
    #[error("identifier must not exceed {max} bytes")]
    TooLong { max: usize },
    #[error("identifier contains whitespace or control characters: {value:?}")]
    InvalidCharacter { value: String },
    #[error("service identifier must use the form `name@version`: {value:?}")]
    InvalidServiceId { value: String },
    #[error("service version must be a positive integer")]
    InvalidServiceVersion,
}

fn validate_token(kind: &'static str, value: &str) -> Result<(), IdError> {
    if value.is_empty() {
        return Err(IdError::Empty);
    }
    if value.len() > MAX_ID_LEN {
        return Err(IdError::TooLong { max: MAX_ID_LEN });
    }
    for ch in value.chars() {
        let ok = ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-' | ':' | '/' | '@');
        if !ok {
            return Err(IdError::InvalidCharacter { value: format!("{kind}: {value}") });
        }
    }
    Ok(())
}

macro_rules! id_newtype {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, JsonSchema)]
        pub struct $name(String);

        impl $name {
            /// Build a validated identifier.
            pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
                let value = value.into();
                validate_token(stringify!($name), &value)?;
                Ok(Self(value))
            }

            /// Borrow the identifier text.
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Consume the identifier and return its text.
            pub fn into_string(self) -> String {
                self.0
            }
        }

            impl AsRef<str> for $name {
                fn as_ref(&self) -> &str {
                    self.as_str()
                }
            }

            impl fmt::Display for $name {
                fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    f.write_str(&self.0)
                }
            }

            impl FromStr for $name {
                type Err = IdError;

                fn from_str(value: &str) -> Result<Self, Self::Err> {
                    Self::new(value)
                }
            }

            impl Serialize for $name {
                fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                    serializer.serialize_str(&self.0)
                }
            }

            impl<'de> Deserialize<'de> for $name {
                fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                    struct IdVisitor;

                    impl Visitor<'_> for IdVisitor {
                        type Value = $name;

                        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                            write!(f, "a validated {} string", stringify!($name))
                        }

                        fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                            Self::Value::new(value).map_err(E::custom)
                        }
                    }

                    deserializer.deserialize_str(IdVisitor)
                }
            }
        };
    }

id_newtype!(
    /// Identifier for a plugin package.
    PluginId
);
id_newtype!(
    /// Identifier for one loaded instance of a plugin package.
    PluginInstanceId
);
id_newtype!(
    /// Identifier for a plugin scope.
    ScopeId
);
id_newtype!(
    /// Identifier for one declarative contribution.
    ContributionId
);
id_newtype!(
    /// Identifier for a command registered by a plugin.
    CommandId
);

/// Stable, versioned service identifier rendered as `name@version`.
///
/// Rendering and parsing always include the version so manifests and
/// diagnostics cannot silently depend on an unversioned name.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, JsonSchema)]
pub struct ServiceId(String);

impl ServiceId {
    /// Build a service identifier from a name and a positive version.
    pub fn new(name: impl AsRef<str>, version: u32) -> Result<Self, IdError> {
        let name = name.as_ref();
        validate_token("ServiceId", name)?;
        if version == 0 {
            return Err(IdError::InvalidServiceVersion);
        }
        Ok(Self(format!("{name}@{version}")))
    }

    /// Parse a static service identifier, panicking on programmer error.
    ///
    /// Intended for `Service::ID` constants and tests, where an invalid literal
    /// is a build-time bug rather than user input.
    pub fn from_static(value: &'static str) -> Self {
        value.parse().expect("invalid static service id")
    }

    /// Parse a static service identifier without panicking.
    pub fn try_from_static(value: &'static str) -> Result<Self, IdError> {
        value.parse()
    }

    /// Name portion, without the version suffix.
    pub fn name(&self) -> &str {
        let (name, _) = self.0.rsplit_once('@').expect("validated service id");
        name
    }

    /// Version suffix.
    pub fn version(&self) -> u32 {
        let (_, version) = self.0.rsplit_once('@').expect("validated service id");
        version.parse().expect("validated service version")
    }

    /// Borrow the full rendered identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ServiceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for ServiceId {
    type Err = IdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (name, version) = value
            .rsplit_once('@')
            .ok_or_else(|| IdError::InvalidServiceId { value: value.to_owned() })?;
        if name.is_empty() || version.is_empty() {
            return Err(IdError::InvalidServiceId { value: value.to_owned() });
        }
        validate_token("ServiceId", name)?;
        let version: u32 =
            version.parse().map_err(|_| IdError::InvalidServiceId { value: value.to_owned() })?;
        if version == 0 {
            return Err(IdError::InvalidServiceVersion);
        }
        Ok(Self(format!("{name}@{version}")))
    }
}

impl Serialize for ServiceId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ServiceId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_id_renders_version() {
        let id = ServiceId::new("editor.document", 1).unwrap();
        assert_eq!(id.to_string(), "editor.document@1");
        assert_eq!(id.name(), "editor.document");
        assert_eq!(id.version(), 1);
    }

    #[test]
    fn service_id_round_trips_through_display_and_parse() {
        for raw in ["editor.document@1", "agent.tools@7", "editor.lsp-read@2"] {
            let parsed: ServiceId = raw.parse().unwrap();
            assert_eq!(parsed.to_string(), raw);
        }
    }

    #[test]
    fn service_id_rejects_invalid_shapes() {
        assert_eq!(
            "editor.document".parse::<ServiceId>(),
            Err(IdError::InvalidServiceId { value: "editor.document".to_owned() })
        );
        assert_eq!("editor.document@0".parse::<ServiceId>(), Err(IdError::InvalidServiceVersion));
        assert!(matches!(
            "editor document@1".parse::<ServiceId>(),
            Err(IdError::InvalidCharacter { .. })
        ));
        assert!(matches!("@1".parse::<ServiceId>(), Err(IdError::InvalidServiceId { .. })));
    }

    #[test]
    fn service_id_rejects_zero_version_on_construction() {
        assert_eq!(ServiceId::new("editor.document", 0), Err(IdError::InvalidServiceVersion));
    }

    #[test]
    fn command_id_rejects_empty_and_control_characters() {
        assert_eq!(CommandId::new(""), Err(IdError::Empty));
        assert!(matches!(CommandId::new("outline\ntoggle"), Err(IdError::InvalidCharacter { .. })));
    }

    #[test]
    fn simple_ids_round_trip_json() {
        let id = CommandId::new("outline.toggle").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"outline.toggle\"");
        assert_eq!(serde_json::from_str::<CommandId>(&json).unwrap(), id);
    }

    #[test]
    fn service_id_round_trips_json() {
        let id = ServiceId::new("agent.tools", 2).unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"agent.tools@2\"");
        assert_eq!(serde_json::from_str::<ServiceId>(&json).unwrap(), id);
    }

    #[test]
    fn json_deserialization_validates() {
        assert!(serde_json::from_str::<CommandId>("\"\"").is_err());
        assert!(serde_json::from_str::<ServiceId>("\"editor.document\"").is_err());
    }

    #[test]
    fn ids_are_ordered() {
        let a = PluginId::new("a").unwrap();
        let b = PluginId::new("b").unwrap();
        assert!(a < b);
    }
}
