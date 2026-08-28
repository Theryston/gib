use jsonschema::Validator;
use schemars::{JsonSchema, SchemaGenerator};
use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, RwLock};

/// Maximum serialized schema size accepted by the local structured-output
/// compiler. Schemas are application contracts, not a way to send arbitrary
/// documents to the model.
pub(crate) const DEFAULT_MAX_SCHEMA_BYTES: usize = 64 * 1024;
/// Maximum schema nesting accepted by the local compiler.
pub(crate) const DEFAULT_MAX_SCHEMA_DEPTH: usize = 32;
/// Maximum number of schema nodes accepted by the local compiler.
pub(crate) const DEFAULT_MAX_SCHEMA_NODES: usize = 512;

/// A stable schema identifier and semantic version pair.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(deny_unknown_fields)]
pub(crate) struct SchemaKey {
    pub(crate) id: String,
    pub(crate) version: String,
}

impl SchemaKey {
    pub(crate) fn new(
        id: impl Into<String>,
        version: impl Into<String>,
    ) -> Result<Self, SchemaError> {
        let key = Self {
            id: id.into(),
            version: version.into(),
        };
        validate_schema_key(&key)?;
        Ok(key)
    }
}

/// Limits and policy used when registering a schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SchemaRegistrationOptions {
    pub(crate) allow_additional_properties: bool,
    pub(crate) max_bytes: usize,
    pub(crate) max_depth: usize,
    pub(crate) max_nodes: usize,
}

impl Default for SchemaRegistrationOptions {
    fn default() -> Self {
        Self {
            allow_additional_properties: false,
            max_bytes: DEFAULT_MAX_SCHEMA_BYTES,
            max_depth: DEFAULT_MAX_SCHEMA_DEPTH,
            max_nodes: DEFAULT_MAX_SCHEMA_NODES,
        }
    }
}

impl SchemaRegistrationOptions {
    #[cfg(test)]
    fn with_limits(max_bytes: usize, max_depth: usize, max_nodes: usize) -> Self {
        Self {
            max_bytes,
            max_depth,
            max_nodes,
            ..Self::default()
        }
    }
}

/// Errors produced before a schema can reach a model or validator.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "code", content = "details", rename_all = "snake_case")]
pub(crate) enum SchemaError {
    InvalidIdentifier,
    InvalidVersion,
    SchemaTooLarge { limit: usize, actual: usize },
    SchemaTooDeep { limit: usize },
    SchemaTooComplex { limit: usize },
    ExternalReference,
    UnresolvedReference,
    UnsupportedKeyword { keyword: String },
    AdditionalPropertiesNotAllowed,
    InvalidSchema,
    ValidatorBuildFailed,
    GrammarBuildFailed,
    SchemaNotFound { id: String, version: String },
    RegistryPoisoned,
}

impl SchemaError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::InvalidIdentifier => "invalid_identifier",
            Self::InvalidVersion => "invalid_version",
            Self::SchemaTooLarge { .. } => "schema_too_large",
            Self::SchemaTooDeep { .. } => "schema_too_deep",
            Self::SchemaTooComplex { .. } => "schema_too_complex",
            Self::ExternalReference => "external_reference",
            Self::UnresolvedReference => "unresolved_reference",
            Self::UnsupportedKeyword { .. } => "unsupported_keyword",
            Self::AdditionalPropertiesNotAllowed => "additional_properties_not_allowed",
            Self::InvalidSchema => "invalid_schema",
            Self::ValidatorBuildFailed => "validator_build_failed",
            Self::GrammarBuildFailed => "grammar_build_failed",
            Self::SchemaNotFound { .. } => "schema_not_found",
            Self::RegistryPoisoned => "registry_poisoned",
        }
    }
}

impl fmt::Display for SchemaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidIdentifier => {
                formatter.write_str("schema ID must use stable lowercase identifier characters")
            }
            Self::InvalidVersion => {
                formatter.write_str("schema version must be semantic major.minor.patch")
            }
            Self::SchemaTooLarge { limit, actual } => {
                write!(formatter, "schema is {actual} bytes; the limit is {limit} bytes")
            }
            Self::SchemaTooDeep { limit } => {
                write!(formatter, "schema nesting exceeds the limit of {limit} levels")
            }
            Self::SchemaTooComplex { limit } => {
                write!(formatter, "schema exceeds the limit of {limit} nodes")
            }
            Self::ExternalReference => {
                formatter.write_str("external JSON Schema references are disabled")
            }
            Self::UnresolvedReference => {
                formatter.write_str("JSON Schema contains an unresolved local reference")
            }
            Self::UnsupportedKeyword { keyword } => {
                write!(
                    formatter,
                    "JSON Schema keyword '{keyword}' is not supported by the local grammar compiler"
                )
            }
            Self::AdditionalPropertiesNotAllowed => {
                formatter.write_str(
                    "additionalProperties must be false unless the schema explicitly opts into extensions",
                )
            }
            Self::InvalidSchema => formatter.write_str("the JSON Schema definition is invalid"),
            Self::ValidatorBuildFailed => {
                formatter.write_str("the local JSON Schema validator could not be built")
            }
            Self::GrammarBuildFailed => {
                formatter.write_str("the local llama.cpp grammar could not be built")
            }
            Self::SchemaNotFound { id, version } => {
                write!(formatter, "schema '{id}' version '{version}' is not registered")
            }
            Self::RegistryPoisoned => formatter.write_str("the schema registry is unavailable"),
        }
    }
}

impl std::error::Error for SchemaError {}

/// A compiled, immutable schema contract shared by prompt and generation
/// requests.
pub(crate) struct SchemaDefinition {
    key: SchemaKey,
    schema: Arc<Value>,
    schema_json: Arc<str>,
    grammar: Arc<str>,
    validator: Arc<Validator>,
    hash: String,
}

impl fmt::Debug for SchemaDefinition {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SchemaDefinition")
            .field("key", &self.key)
            .field("schema_bytes", &self.schema_json.len())
            .field("hash", &self.hash)
            .finish()
    }
}

impl SchemaDefinition {
    pub(crate) fn key(&self) -> &SchemaKey {
        &self.key
    }

    pub(crate) fn schema(&self) -> &Value {
        &self.schema
    }

    pub(crate) fn schema_json(&self) -> &str {
        &self.schema_json
    }

    pub(crate) fn grammar(&self) -> &str {
        &self.grammar
    }

    pub(crate) fn validator(&self) -> &Validator {
        &self.validator
    }

    pub(crate) fn hash(&self) -> &str {
        &self.hash
    }
}

/// Thread-safe local registry of compiled JSON Schema contracts.
#[derive(Clone, Default)]
pub(crate) struct SchemaRegistry {
    schemas: Arc<RwLock<BTreeMap<SchemaKey, Arc<SchemaDefinition>>>>,
}

impl fmt::Debug for SchemaRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let count = self
            .schemas
            .read()
            .map(|schemas| schemas.len())
            .unwrap_or(0);
        formatter
            .debug_struct("SchemaRegistry")
            .field("count", &count)
            .finish()
    }
}

impl SchemaRegistry {
    /// Register a Rust DTO using the deterministic default Schemars settings.
    pub(crate) fn register<T: JsonSchema>(
        &self,
        id: impl Into<String>,
        version: impl Into<String>,
    ) -> Result<SchemaKey, SchemaError> {
        self.register_with_options::<T>(id, version, SchemaRegistrationOptions::default())
    }

    /// Register a Rust DTO with explicit compiler limits and extension policy.
    pub(crate) fn register_with_options<T: JsonSchema>(
        &self,
        id: impl Into<String>,
        version: impl Into<String>,
        options: SchemaRegistrationOptions,
    ) -> Result<SchemaKey, SchemaError> {
        let schema = SchemaGenerator::default().into_root_schema_for::<T>();
        let value = serde_json::to_value(schema).map_err(|_| SchemaError::InvalidSchema)?;
        self.register_value_with_options(id, version, value, options)
    }

    /// Register a checked-in or manually authored schema value.
    pub(crate) fn register_value(
        &self,
        id: impl Into<String>,
        version: impl Into<String>,
        schema: Value,
    ) -> Result<SchemaKey, SchemaError> {
        self.register_value_with_options(id, version, schema, SchemaRegistrationOptions::default())
    }

    /// Register a schema value with explicit policy and resource limits.
    pub(crate) fn register_value_with_options(
        &self,
        id: impl Into<String>,
        version: impl Into<String>,
        mut schema: Value,
        options: SchemaRegistrationOptions,
    ) -> Result<SchemaKey, SchemaError> {
        let key = SchemaKey::new(id, version)?;
        if options.max_bytes == 0 || options.max_depth == 0 || options.max_nodes == 0 {
            return Err(SchemaError::InvalidSchema);
        }

        let original_size = serde_json::to_vec(&schema)
            .map_err(|_| SchemaError::InvalidSchema)?
            .len();
        if original_size > options.max_bytes {
            return Err(SchemaError::SchemaTooLarge {
                limit: options.max_bytes,
                actual: original_size,
            });
        }
        let original_schema = schema.clone();
        let mut stats = SchemaStats::default();
        normalize_and_validate_schema(&mut schema, &original_schema, 0, &mut stats, options)?;
        let schema_json = serde_json::to_string(&schema).map_err(|_| SchemaError::InvalidSchema)?;
        if schema_json.len() > options.max_bytes {
            return Err(SchemaError::SchemaTooLarge {
                limit: options.max_bytes,
                actual: schema_json.len(),
            });
        }

        let validator =
            jsonschema::validator_for(&schema).map_err(|_| SchemaError::ValidatorBuildFailed)?;
        let grammar = llama_cpp_2::json_schema_to_grammar(&schema_json)
            .map_err(|_| SchemaError::GrammarBuildFailed)?;
        if grammar.trim().is_empty() || !grammar.lines().any(|line| line.starts_with("root ::= ")) {
            return Err(SchemaError::GrammarBuildFailed);
        }

        let hash = sha256_hex(schema_json.as_bytes());
        let definition = Arc::new(SchemaDefinition {
            key: key.clone(),
            schema: Arc::new(schema),
            schema_json: Arc::from(schema_json),
            grammar: Arc::from(grammar),
            validator: Arc::new(validator),
            hash,
        });
        let mut schemas = self
            .schemas
            .write()
            .map_err(|_| SchemaError::RegistryPoisoned)?;
        schemas.insert(key.clone(), definition);
        Ok(key)
    }

    pub(crate) fn get(&self, key: &SchemaKey) -> Result<Arc<SchemaDefinition>, SchemaError> {
        let schemas = self
            .schemas
            .read()
            .map_err(|_| SchemaError::RegistryPoisoned)?;
        schemas
            .get(key)
            .cloned()
            .ok_or_else(|| SchemaError::SchemaNotFound {
                id: key.id.clone(),
                version: key.version.clone(),
            })
    }

    pub(crate) fn contains(&self, key: &SchemaKey) -> Result<bool, SchemaError> {
        let schemas = self
            .schemas
            .read()
            .map_err(|_| SchemaError::RegistryPoisoned)?;
        Ok(schemas.contains_key(key))
    }
}

#[derive(Default)]
struct SchemaStats {
    nodes: usize,
    deepest: usize,
}

fn validate_schema_key(key: &SchemaKey) -> Result<(), SchemaError> {
    if !is_stable_identifier(&key.id) {
        return Err(SchemaError::InvalidIdentifier);
    }
    if !is_semantic_version(&key.version) {
        return Err(SchemaError::InvalidVersion);
    }
    Ok(())
}

fn is_stable_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_lowercase())
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
}

fn is_semantic_version(value: &str) -> bool {
    value.len() <= 128 && Version::parse(value).is_ok()
}

fn normalize_and_validate_schema(
    value: &mut Value,
    root: &Value,
    depth: usize,
    stats: &mut SchemaStats,
    options: SchemaRegistrationOptions,
) -> Result<(), SchemaError> {
    stats.nodes = stats.nodes.saturating_add(1);
    stats.deepest = stats.deepest.max(depth);
    if stats.nodes > options.max_nodes {
        return Err(SchemaError::SchemaTooComplex {
            limit: options.max_nodes,
        });
    }
    if depth > options.max_depth {
        return Err(SchemaError::SchemaTooDeep {
            limit: options.max_depth,
        });
    }

    let Value::Object(object) = value else {
        if value.is_boolean() {
            return Ok(());
        }
        return Err(SchemaError::InvalidSchema);
    };

    lower_lossless_primitive_formats(object)?;
    validate_keyword_set(object)?;
    validate_grammar_constraints(object)?;
    if let Some(meta_schema) = object.get("$schema") {
        let Some(meta_schema) = meta_schema.as_str() else {
            return Err(SchemaError::InvalidSchema);
        };
        if !matches!(
            meta_schema,
            "https://json-schema.org/draft/2020-12/schema"
                | "https://json-schema.org/draft/2020-12/schema#"
                | "http://json-schema.org/draft-07/schema#"
                | "http://json-schema.org/draft-07/schema"
        ) {
            return Err(SchemaError::ExternalReference);
        }
    }
    if let Some(reference) = object.get("$ref") {
        let Some(reference) = reference.as_str() else {
            return Err(SchemaError::InvalidSchema);
        };
        if !reference.starts_with('#') {
            return Err(SchemaError::ExternalReference);
        }
        if reference != "#" && !reference.starts_with("#/") {
            return Err(SchemaError::InvalidSchema);
        }
        if reference != "#" && root.pointer(&reference[1..]).is_none() {
            return Err(SchemaError::UnresolvedReference);
        }
    }

    let object_schema = is_object_schema(object);
    if object_schema {
        match object.get("additionalProperties") {
            None => {
                object.insert("additionalProperties".to_string(), Value::Bool(false));
            }
            Some(Value::Bool(false)) => {}
            Some(_) if !options.allow_additional_properties => {
                return Err(SchemaError::AdditionalPropertiesNotAllowed);
            }
            Some(_) => {}
        }
    }

    let schema_children = [
        "$defs",
        "definitions",
        "properties",
        "additionalProperties",
        "items",
        "oneOf",
        "anyOf",
        "contains",
        "propertyNames",
        "if",
        "then",
        "else",
        "not",
    ];
    for keyword in schema_children {
        let Some(child) = object.get_mut(keyword) else {
            continue;
        };
        match keyword {
            "$defs" | "definitions" | "properties" => {
                let Value::Object(children) = child else {
                    return Err(SchemaError::InvalidSchema);
                };
                for nested in children.values_mut() {
                    normalize_and_validate_schema(nested, root, depth + 1, stats, options)?;
                }
            }
            "oneOf" | "anyOf" => {
                let Value::Array(children) = child else {
                    return Err(SchemaError::InvalidSchema);
                };
                for nested in children {
                    normalize_and_validate_schema(nested, root, depth + 1, stats, options)?;
                }
            }
            _ => normalize_and_validate_schema(child, root, depth + 1, stats, options)?,
        }
    }
    Ok(())
}

fn is_object_schema(object: &Map<String, Value>) -> bool {
    object.contains_key("properties")
        || object.contains_key("required")
        || object.contains_key("additionalProperties")
        || object.get("type").is_some_and(|value| match value {
            Value::String(value) => value == "object",
            Value::Array(values) => values.iter().any(|value| value.as_str() == Some("object")),
            _ => false,
        })
}

fn validate_keyword_set(object: &Map<String, Value>) -> Result<(), SchemaError> {
    for keyword in object.keys() {
        let supported = matches!(
            keyword.as_str(),
            "$schema"
                | "$id"
                | "$ref"
                | "$defs"
                | "definitions"
                | "title"
                | "description"
                | "$comment"
                | "default"
                | "examples"
                | "deprecated"
                | "readOnly"
                | "writeOnly"
                | "type"
                | "enum"
                | "const"
                | "oneOf"
                | "anyOf"
                | "properties"
                | "required"
                | "additionalProperties"
                | "items"
                | "minItems"
                | "maxItems"
                | "pattern"
                | "minLength"
                | "maxLength"
                | "format"
                | "minimum"
                | "maximum"
                | "exclusiveMinimum"
                | "exclusiveMaximum"
        );
        if !supported {
            return Err(SchemaError::UnsupportedKeyword {
                keyword: keyword.clone(),
            });
        }
    }

    if let Some(format) = object.get("format") {
        let supported = format.as_str().is_some_and(|format| {
            matches!(
                format,
                "date"
                    | "time"
                    | "date-time"
                    | "uuid"
                    | "uuid1"
                    | "uuid2"
                    | "uuid3"
                    | "uuid4"
                    | "uuid5"
            )
        });
        if !supported {
            return Err(SchemaError::UnsupportedKeyword {
                keyword: "format".to_string(),
            });
        }
    }
    Ok(())
}

fn validate_grammar_constraints(object: &Map<String, Value>) -> Result<(), SchemaError> {
    if let Some(required) = object.get("required") {
        let (Value::Array(required), Some(Value::Object(properties))) =
            (required, object.get("properties"))
        else {
            return Err(SchemaError::InvalidSchema);
        };
        if required.iter().any(|name| {
            name.as_str()
                .is_none_or(|name| !properties.contains_key(name))
        }) {
            return Err(SchemaError::InvalidSchema);
        }
    }
    if (object.contains_key("minItems") || object.contains_key("maxItems"))
        && !object.contains_key("items")
    {
        return Err(SchemaError::InvalidSchema);
    }
    if (object.contains_key("minLength") || object.contains_key("maxLength"))
        && object.get("type").and_then(Value::as_str) != Some("string")
    {
        return Err(SchemaError::InvalidSchema);
    }
    if object.contains_key("pattern")
        && object
            .get("type")
            .is_some_and(|value| !matches!(value, Value::String(value) if value == "string"))
    {
        return Err(SchemaError::InvalidSchema);
    }
    if (object.contains_key("minimum")
        || object.contains_key("maximum")
        || object.contains_key("exclusiveMinimum")
        || object.contains_key("exclusiveMaximum"))
        && object.get("type").and_then(Value::as_str) != Some("integer")
    {
        return Err(SchemaError::UnsupportedKeyword {
            keyword: "numeric_bounds".to_string(),
        });
    }
    if object.contains_key("oneOf") || object.contains_key("anyOf") {
        let has_non_annotation_sibling = object.keys().any(|keyword| {
            !matches!(
                keyword.as_str(),
                "oneOf"
                    | "anyOf"
                    | "$schema"
                    | "$id"
                    | "title"
                    | "description"
                    | "$comment"
                    | "default"
                    | "examples"
                    | "deprecated"
                    | "readOnly"
                    | "writeOnly"
            )
        });
        if has_non_annotation_sibling {
            return Err(SchemaError::InvalidSchema);
        }
    }
    if object.contains_key("enum") || object.contains_key("const") {
        let has_non_annotation_sibling = object.keys().any(|keyword| {
            !matches!(
                keyword.as_str(),
                "enum"
                    | "const"
                    | "type"
                    | "$schema"
                    | "$id"
                    | "title"
                    | "description"
                    | "$comment"
                    | "default"
                    | "examples"
                    | "deprecated"
                    | "readOnly"
                    | "writeOnly"
            )
        });
        if has_non_annotation_sibling {
            return Err(SchemaError::InvalidSchema);
        }
    }
    if object.contains_key("$ref")
        && object.keys().any(|keyword| {
            keyword != "$ref"
                && !matches!(
                    keyword.as_str(),
                    "title" | "description" | "$comment" | "$id" | "$schema"
                )
        })
    {
        return Err(SchemaError::InvalidSchema);
    }
    Ok(())
}

fn lower_lossless_primitive_formats(object: &mut Map<String, Value>) -> Result<(), SchemaError> {
    let Some(Value::String(format)) = object.get("format") else {
        return if object.contains_key("format") {
            Err(SchemaError::InvalidSchema)
        } else {
            Ok(())
        };
    };
    let primitive_format = matches!(
        format.as_str(),
        "int8"
            | "int16"
            | "int32"
            | "int64"
            | "uint8"
            | "uint16"
            | "uint32"
            | "uint64"
            | "float"
            | "double"
    );
    if !primitive_format {
        return Ok(());
    }
    let compatible_type = object.get("type").is_some_and(|value| match value {
        Value::String(value) => {
            (format.starts_with("int") || format.starts_with("uint")) && value == "integer"
                || (matches!(format.as_str(), "float" | "double") && value == "number")
        }
        Value::Array(values) => values.iter().any(|value| {
            value.as_str().is_some_and(|value| {
                (format.starts_with("int") || format.starts_with("uint")) && value == "integer"
                    || (matches!(format.as_str(), "float" | "double") && value == "number")
            })
        }),
        _ => false,
    });
    if !compatible_type {
        return Err(SchemaError::UnsupportedKeyword {
            keyword: "format".to_string(),
        });
    }
    // Schemars uses formats such as uint32 as lossless Rust-width
    // annotations. JSON Schema validation plus typed deserialization already
    // enforce the actual primitive type, while llama.cpp does not understand
    // these annotations. Removing only this annotation is a documented,
    // semantics-preserving lowering.
    object.remove("format");
    Ok(())
}

fn sha256_hex(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[derive(Debug, Serialize, JsonSchema)]
    struct Example {
        name: String,
        count: u32,
    }

    #[derive(Debug, JsonSchema)]
    enum ExampleKind {
        File,
        Directory,
    }

    #[derive(Debug, JsonSchema)]
    struct NestedExample {
        kind: ExampleKind,
        note: Option<String>,
        values: Vec<u32>,
    }

    #[test]
    fn generated_schema_is_compiled_with_closed_objects() {
        let registry = SchemaRegistry::default();
        let key = registry
            .register::<Example>("example", "1.0.0")
            .expect("schema should compile");
        let definition = registry.get(&key).expect("schema should be available");
        assert_eq!(definition.key(), &key);
        assert_eq!(definition.schema()["additionalProperties"], false);
        assert!(definition.grammar().contains("root ::="));
        assert_eq!(definition.hash().len(), 64);
    }

    #[test]
    fn generated_nested_optional_enum_and_array_schemas_compile() {
        let registry = SchemaRegistry::default();
        let key = registry
            .register::<NestedExample>("nested-example", "1.0.0")
            .expect("nested schema should compile");
        let definition = registry.get(&key).expect("schema should be available");
        assert!(definition.grammar().contains("values"));
        assert!(definition.grammar().contains("File"));
        assert!(definition.validator().is_valid(&json!({
            "kind": "File",
            "note": null,
            "values": [1, 2],
        })));
    }

    #[test]
    fn registry_rejects_external_and_unsupported_contracts() {
        let registry = SchemaRegistry::default();
        let external = registry.register_value(
            "external",
            "1.0.0",
            json!({"$ref": "https://example.com/schema.json"}),
        );
        assert!(matches!(external, Err(SchemaError::ExternalReference)));

        let unsupported = registry.register_value(
            "unsupported",
            "1.0.0",
            json!({"type": "array", "items": {"type": "string"}, "uniqueItems": true}),
        );
        assert!(matches!(
            unsupported,
            Err(SchemaError::UnsupportedKeyword { keyword }) if keyword == "uniqueItems"
        ));
    }

    #[test]
    fn registry_enforces_size_depth_and_extension_limits() {
        let registry = SchemaRegistry::default();
        let oversized = registry.register_value_with_options(
            "oversized",
            "1.0.0",
            json!({"type": "string", "description": "x".repeat(256)}),
            SchemaRegistrationOptions::with_limits(128, 32, 512),
        );
        assert!(matches!(oversized, Err(SchemaError::SchemaTooLarge { .. })));

        let mut deep = json!({"type": "string"});
        for _ in 0..4 {
            deep = json!({"anyOf": [deep]});
        }
        let too_deep = registry.register_value_with_options(
            "deep",
            "1.0.0",
            deep,
            SchemaRegistrationOptions::with_limits(64 * 1024, 2, 512),
        );
        assert!(matches!(too_deep, Err(SchemaError::SchemaTooDeep { .. })));

        let open = registry.register_value(
            "open",
            "1.0.0",
            json!({"type": "object", "additionalProperties": true}),
        );
        assert!(matches!(
            open,
            Err(SchemaError::AdditionalPropertiesNotAllowed)
        ));
    }

    #[test]
    fn grammar_conversion_preserves_supported_object_array_enum_and_required_constraints() {
        let registry = SchemaRegistry::default();
        let object = registry
            .register_value(
                "supported-contract",
                "1.0.0",
                json!({
                    "type": "object",
                    "properties": {
                        "kind": {"type": "string", "enum": ["file", "directory"]},
                        "revisions": {
                            "type": "array",
                            "items": {"type": "integer"},
                            "minItems": 1,
                            "maxItems": 3
                        }
                    },
                    "required": ["kind", "revisions"],
                    "additionalProperties": false
                }),
            )
            .expect("supported schema should compile");
        let definition = registry.get(&object).expect("schema should be available");
        assert!(definition.grammar().contains("file"));
        assert!(definition.grammar().contains("directory"));
        assert!(definition.grammar().contains("revisions"));
        assert!(!definition.validator().is_valid(&json!({
            "kind": "file",
            "revisions": [],
        })));
        assert!(!definition.validator().is_valid(&json!({
            "kind": "file",
            "revisions": [1],
            "unexpected": true,
        })));
        assert!(definition.validator().is_valid(&json!({
            "kind": "directory",
            "revisions": [1, 2],
        })));
    }

    #[test]
    fn local_references_are_resolved_without_network_access() {
        let registry = SchemaRegistry::default();
        let key = registry
            .register_value(
                "local-reference",
                "1.0.0",
                json!({
                    "type": "object",
                    "properties": {
                        "child": {"$ref": "#/$defs/Child"}
                    },
                    "required": ["child"],
                    "$defs": {
                        "Child": {
                            "type": "object",
                            "properties": {"name": {"type": "string"}},
                            "required": ["name"]
                        }
                    }
                }),
            )
            .expect("local schema references should compile");
        let definition = registry.get(&key).expect("schema should be available");
        assert!(
            definition
                .validator()
                .is_valid(&json!({"child": {"name": "ok"}}))
        );
        assert!(!definition.validator().is_valid(&json!({"child": {}})));
        assert!(definition.grammar().contains("child"));
    }

    #[test]
    fn invalid_schema_definitions_fail_before_model_use() {
        let registry = SchemaRegistry::default();
        let invalid = registry.register_value(
            "invalid-definition",
            "1.0.0",
            json!({"type": "not-a-json-schema-type"}),
        );
        assert!(matches!(
            invalid,
            Err(SchemaError::ValidatorBuildFailed | SchemaError::GrammarBuildFailed)
        ));
    }

    #[test]
    fn schema_keys_are_strictly_versioned() {
        assert!(matches!(
            SchemaKey::new("NotStable", "1.0.0"),
            Err(SchemaError::InvalidIdentifier)
        ));
        assert!(matches!(
            SchemaKey::new("stable", "1.0"),
            Err(SchemaError::InvalidVersion)
        ));
    }
}
