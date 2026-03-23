//! A library for parsing PostgreSQL enum types from SQL definitions and representing them in Rust.
//! This is intended to be used as a helper for code generation in Ruby projects, but it can be
//! used anywhere you might want to statically analyze PostgreSQL enum types.

use std::collections::{HashMap, HashSet};
use std::fs;

use pg_query::{parse, protobuf::ObjectType};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors that can occur during parsing and processing of PostgreSQL enum types.
#[derive(Debug, Error)]
pub enum PGEnumError {
    /// An error indicating that an enum type is duplicated within the same schema.
    /// This will not happen with valid SQL, but it can occur if someone is manually editing
    /// the inputs, RON representation, etc.
    #[error("duplicated enum type '{enum_name}' in schema '{schema}'")]
    DuplicatedType {
        schema: String,
        enum_name: String,
    },
    /// An error indicating that a value in an enum is duplicated within the same schema.
    /// This will not happen with valid SQL, but it can occur if someone is manually editing
    /// the inputs, RON representation, etc.
    #[error("duplicated value '{value}' in enum '{enum_name}' (schema: '{schema}')")]
    DuplicatedValue {
        schema: String,
        enum_name: String,
        value: String,
    },
    /// An error indicating that a name of an enum type is not unique within
    /// the same `Database`. This will occur if the same type is defined
    /// in multiple schemas.
    #[error("enum '{name}' exists in multiple schemas: {schemas:?}")]
    EnumConflict { name: String, schemas: Vec<String> },
    /// An error indicating that an enum type is not found.
    #[error("enum '{name}' not found")]
    EnumNotFound { name: String, schema: Option<String> },
    /// An error generated when the RON representation cannot be parsed.
    #[error("invalid RON representation")]
    InvalidRepresentation(#[source] ron::error::SpannedError),
    /// An error generated when a file IO operation fails.
    #[error("IO error: {0}")]
    IOError(#[from] std::io::Error),
    /// An error generated when the SQL information cannot be parsed.
    #[error("SQL parse error")]
    SQLParseError(pg_query::Error),
    /// An error generated when the RON representation parser is provided
    /// with an unknown / unsupported version number.
    #[error("unknown version: {0}")]
    UnknownVersion(u32),
}

/// The current version of the RON serialization format.
/// This is not likely to change any time soon, but it's included
/// for future-proofing.
pub const RON_VERSION: u32 = 1;

/// A representation of a PostgreSQL enum type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnumType {
    schema: String,
    name: String,
    comment: Option<String>,
    values: Vec<String>,
    #[serde(skip)]
    digest: String,
}

impl EnumType {
    /// Get the comment (if present) for this enum type.
    pub fn comment(&self) -> Option<&str> {
        self.comment.as_deref()
    }

    /// Compute a digest of the enum values for quick comparison.
    /// This is not intended to be a secure hash, just a way to quickly check if the values have changed.
    fn compute_digest(schema: &str, name: &str, values: &[String]) -> String {
        use std::hash::{Hash, Hasher};

        let mut hasher = std::collections::hash_map::DefaultHasher::new();

        schema.hash(&mut hasher);
        name.hash(&mut hasher);
        values.hash(&mut hasher);

        format!("{:x}", hasher.finish())
    }

    /// Read the digest of the enum values. This should serve as a unique-enough identifier for any given
    /// enum, for the sake of caching and quick comparisons.
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// Retrieve the name of this enum type.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Build a new `EnumType` instance, computing the digest of the values for later comparison.
    pub fn new(schema: String, name: String, comment: Option<String>, values: Vec<String>) -> Self {
        let digest = Self::compute_digest(&schema, &name, &values);

        EnumType {
            schema,
            name,
            comment,
            values,
            digest,
        }
    }

    /// Retrieve the schema of this enum type.
    pub fn schema(&self) -> &str {
        &self.schema
    }
}

impl PartialEq for EnumType {
    fn eq(&self, other: &Self) -> bool {
        self.digest == other.digest
    }
}

impl Eq for EnumType {}

#[derive(Debug, Clone)]
enum EnumLookup {
    /// The index of the enum in the `enums` vector if found unambiguously
    Found(usize),
    // List of schemas where the name was found
    Conflict(Vec<String>),
}

type SchemalessLookup = HashMap<String, EnumLookup>;

type SchemaMap = HashMap<String, HashMap<String, usize>>;

/// The top-level representation of the database structure, containing all enum types
/// and various methods for looking an enum up by name (and optional schema).
#[derive(Debug, Clone)]
pub struct Database {
    /// An identifier for the database. Note that it does not have to match the
    /// actual database name, it's just a label for this set of enums that can
    /// be used to distinguish it from others if needed. For example, in Rails
    /// you may have multiple database connections in a single environment,
    /// such as `primary`, `analytics`, etc. If you are extracting enums from each,
    /// you could use those same labels here.
    name: String,
    /// The list of `EnumType`s defined in this database.
    enums: Vec<EnumType>,
    /// A lookup map for enums by name, without considering schema. This is used to
    /// implement the `get` method that looks up by name without schema, and it also
    /// precomputes conflicts for names that exist in multiple schemas.
    lookup: SchemalessLookup,
    /// A nested map of schema -> enum name -> index in the `enums` vector.
    schemas: SchemaMap,
}

impl PartialEq for Database {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.enums == other.enums
    }
}

impl Eq for Database {}

impl Database {
    /// Get a slice of all enums in this database.
    pub fn enums(&self) -> &[EnumType] {
        &self.enums
    }

    /// Deserialize a RON string into a `Database`.
    ///
    /// The `version` argument is used to allow for future versions of the RON format.
    pub fn from_ron(version: u32, s: &str) -> anyhow::Result<Self> {
        if version != RON_VERSION {
            return Err(PGEnumError::UnknownVersion(version).into());
        }

        let v1: DatabaseV1 = ron::from_str(s).map_err(PGEnumError::InvalidRepresentation)?;

        if v1.version != RON_VERSION {
            return Err(PGEnumError::UnknownVersion(v1.version).into());
        }

        Ok(v1.into())
    }

    /// Read a RON file and deserialize it into a `Database`.
    pub fn from_ron_file(version: u32, path: &str) -> anyhow::Result<Self> {
        let contents = fs::read_to_string(path).map_err(PGEnumError::IOError)?;

        Self::from_ron(version, &contents)
    }

    /// Statically analyze a SQL dump to extract enum declarations and their values.
    ///
    /// It uses default parsing options, which can be customized with `from_sql_with_options`.
    pub fn from_sql(name: &str, sql: &str) -> anyhow::Result<Self> {
        let options = SQLParsingOptions::default();

        Self::from_sql_with_options(name, sql, options)
    }

    /// Statically analyze a SQL dump to extract enum declarations and their values, with options for parsing.
    pub fn from_sql_with_options(name: &str, sql: &str, options: SQLParsingOptions) -> anyhow::Result<Self> {
        let result = parse(sql).map_err(PGEnumError::SQLParseError)?;

        let mut builder = DatabaseBuilder::new(name, options);

        for raw_stmt in &result.protobuf.stmts {
            let Some(node) = raw_stmt.stmt.as_ref().and_then(|n| n.node.as_ref()) else {
                continue;
            };

            match node {
                pg_query::protobuf::node::Node::CreateEnumStmt(stmt) => {
                    let (schema, type_name) = extract_type_name(&stmt.type_name);
                    let schema_key = schema.unwrap_or_default();

                    let values: Vec<String> = stmt.vals.iter().filter_map(node_to_string).collect();
                    builder.add_enum(&schema_key, &type_name, values, None)?;
                }
                pg_query::protobuf::node::Node::CommentStmt(stmt) => {
                    if stmt.objtype != ObjectType::ObjectType as i32 {
                        continue;
                    }
                    let Some(obj_node) = stmt.object.as_ref().and_then(|n| n.node.as_ref()) else {
                        continue;
                    };
                    let type_name_nodes = match obj_node {
                        pg_query::protobuf::node::Node::TypeName(tn) => &tn.names,
                        _ => continue,
                    };
                    let (schema, type_name) = extract_type_name(type_name_nodes);
                    let schema_key = schema.unwrap_or_default();
                    builder.set_comment(&schema_key, &type_name, &stmt.comment);
                }
                _ => {}
            }
        }

        Ok(builder.into())
    }

    /// Read a SQL file and parse it to extract enum declarations.
    pub fn from_sql_file(name: &str, path: &str) -> anyhow::Result<Self> {
        let contents = fs::read_to_string(path).map_err(PGEnumError::IOError)?;

        Self::from_sql(name, &contents)
    }

    /// Look up an enum by name, optionally qualified by schema.
    /// If `schema` is `None`, this will attempt to find an enum with the given name
    /// that is unique across all schemas. If there's a conflict, an `EnumConflict`
    /// error will be returned. If no enum with the given name is found, an `EnumNotFound`
    /// error will be returned.
    ///
    /// If `schema` is provided, it will look for an enum with the given
    /// name within that schema. If the enum is not found, an `EnumNotFound`
    /// error will be returned.
    pub fn get(&self, schema: Option<&str>, name: &str) -> anyhow::Result<&EnumType> {
        match schema {
            Some(s) => self
                .schemas
                .get(s)
                .and_then(|m| m.get(name))
                .map(|&idx| &self.enums[idx])
                .ok_or_else(|| {
                    PGEnumError::EnumNotFound {
                        name: name.to_string(),
                        schema: Some(s.to_string()),
                    }
                    .into()
                }),
            None => match self.lookup.get(name) {
                Some(EnumLookup::Found(idx)) => Ok(&self.enums[*idx]),
                Some(EnumLookup::Conflict(schemas)) => Err(PGEnumError::EnumConflict {
                    name: name.to_string(),
                    schemas: schemas.clone(),
                }
                .into()),
                None => Err(PGEnumError::EnumNotFound {
                    name: name.to_string(),
                    schema: None,
                }
                .into()),
            },
        }
    }

    /// Get the name of this database.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Serialize to a RON string.
    pub fn to_ron(&self) -> anyhow::Result<String> {
        let v1 = DatabaseV1 {
            version: RON_VERSION,
            name: self.name.clone(),
            enums: self.enums.clone(),
        };

        let config = ron::ser::PrettyConfig::default();

        Ok(ron::ser::to_string_pretty(&v1, config)?)
    }

    /// Serialize to a RON string and write it to a file.
    pub fn to_ron_file(&self, path: &str) -> anyhow::Result<()> {
        let ron_str = self.to_ron()?;

        fs::write(path, ron_str).map_err(PGEnumError::IOError)?;

        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct SQLParsingOptions {
    default_schema: String,
}

impl Default for SQLParsingOptions {
    fn default() -> Self {
        Self {
            default_schema: "public".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct DatabaseV1 {
    version: u32,
    name: String,
    enums: Vec<EnumType>,
}

fn build_indexes(
    enums: &[EnumType],
) -> (
    SchemalessLookup,
    SchemaMap,
) {
    let mut lookup: SchemalessLookup = HashMap::new();
    let mut schemas: SchemaMap = HashMap::new();

    for (i, e) in enums.iter().enumerate() {
        schemas
            .entry(e.schema.clone())
            .or_default()
            .insert(e.name.clone(), i);

        lookup
            .entry(e.name.clone())
            .and_modify(|existing| match existing {
                EnumLookup::Found(prev_idx) => {
                    *existing =
                        EnumLookup::Conflict(vec![enums[*prev_idx].schema.clone(), e.schema.clone()]);
                }
                EnumLookup::Conflict(schemas) => {
                    schemas.push(e.schema.clone());
                }
            })
            .or_insert(EnumLookup::Found(i));
    }

    (lookup, schemas)
}

impl From<Database> for DatabaseV1 {
    fn from(es: Database) -> Self {
        DatabaseV1 {
            version: RON_VERSION,
            name: es.name,
            enums: es.enums,
        }
    }
}

impl From<DatabaseV1> for Database {
    fn from(mut v1: DatabaseV1) -> Self {
        for e in &mut v1.enums {
            e.digest = EnumType::compute_digest(&e.schema, &e.name, &e.values);
        }
        let (lookup, schemas) = build_indexes(&v1.enums);
        Database {
            name: v1.name,
            enums: v1.enums,
            lookup,
            schemas,
        }
    }
}

/// A builder for constructing a `Database` instance incrementally,
/// with checks for duplicate enum types and values.
struct DatabaseBuilder {
    name: String,
    enums: Vec<EnumType>,
    seen_types: HashSet<(String, String)>,
    options: SQLParsingOptions,
}

impl DatabaseBuilder {
    pub fn new(name: &str, options: SQLParsingOptions) -> Self {
        Self {
            name: name.to_string(),
            enums: Vec::new(),
            seen_types: HashSet::new(),
            options,
        }
    }

    pub fn add_enum(
        &mut self,
        schema: &str,
        name: &str,
        values: Vec<String>,
        comment: Option<&str>,
    ) -> anyhow::Result<()> {
        let schema = if schema.is_empty() {
            self.options.default_schema.as_str()
        } else {
            schema
        };

        // Check for duplicate values within this enum
        let mut seen_values = HashSet::new();
        for v in &values {
            if !seen_values.insert(v) {
                return Err(PGEnumError::DuplicatedValue {
                    schema: schema.to_string(),
                    enum_name: name.to_string(),
                    value: v.clone(),
                }
                .into());
            }
        }

        if !self
            .seen_types
            .insert((schema.to_string(), name.to_string()))
        {
            return Err(PGEnumError::DuplicatedType {
                schema: schema.to_string(),
                enum_name: name.to_string(),
            }
            .into());
        }

        self.enums.push(EnumType::new(
            schema.to_string(),
            name.to_string(),
            comment.map(|s| s.to_string()),
            values,
        ));

        Ok(())
    }

    pub fn set_comment(&mut self, schema: &str, name: &str, comment: &str) {
        let schema = if schema.is_empty() {
            self.options.default_schema.as_str()
        } else {
            schema
        };

        if let Some(e) = self
            .enums
            .iter_mut()
            .find(|e| e.schema == schema && e.name == name)
        {
            e.comment = Some(comment.to_string());
        }
    }

}

impl From<DatabaseBuilder> for Database {
    fn from(builder: DatabaseBuilder) -> Self {
        let (lookup, schemas) = build_indexes(&builder.enums);
        Database {
            name: builder.name,
            enums: builder.enums,
            lookup,
            schemas,
        }
    }
}

/// Extract schema and type name from a list of name nodes (e.g. `[public, access_management]`).
fn extract_type_name(nodes: &[pg_query::protobuf::Node]) -> (Option<String>, String) {
    let parts: Vec<String> = nodes.iter().filter_map(node_to_string).collect();
    match parts.len() {
        0 => (None, String::new()),
        1 => (None, parts[0].clone()),
        _ => (Some(parts[0].clone()), parts[parts.len() - 1].clone()),
    }
}

fn node_to_string(node: &pg_query::protobuf::Node) -> Option<String> {
    match node.node.as_ref()? {
        pg_query::protobuf::node::Node::String(s) => Some(s.sval.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_SQL: &str = include_str!("../tests/fixtures/valid_enums.sql");
    const VALID_RON: &str = include_str!("../tests/fixtures/valid_enums_v1.ron");
    const INVALID_SQL: &str = include_str!("../tests/fixtures/invalid.sql");

    #[test]
    fn ron_round_trip() {
        let database = Database::from_sql("test", VALID_SQL).unwrap();
        let ron_str = database.to_ron().unwrap();
        assert!(ron_str.contains("version: 1"));
        let recovered = Database::from_ron(RON_VERSION, &ron_str).unwrap();
        assert_eq!(database, recovered);
    }

    #[test]
    fn from_ron_parses_fixture() {
        let expected = Database::from_sql("test", VALID_SQL).unwrap();
        let parsed = Database::from_ron(RON_VERSION, VALID_RON).unwrap();
        assert_eq!(expected, parsed);
    }

    #[test]
    fn from_ron_returns_error_for_invalid_representation() {
        let err = Database::from_ron(RON_VERSION, "not valid ron").unwrap_err();
        assert!(err
            .downcast_ref::<PGEnumError>()
            .is_some_and(|e| matches!(e, PGEnumError::InvalidRepresentation(_))));
    }

    #[test]
    fn from_ron_rejects_unsupported_version_argument() {
        let err = Database::from_ron(999, VALID_RON).unwrap_err();
        let pgerr = err
            .downcast_ref::<PGEnumError>()
            .expect("expected PGEnumError");
        assert!(
            matches!(pgerr, PGEnumError::UnknownVersion(999)),
            "expected UnknownVersion(999), got: {pgerr:?}"
        );
    }

    #[test]
    fn from_ron_returns_error_for_unknown_version_in_payload() {
        let database = Database::from_sql("test", VALID_SQL).unwrap();
        let mut v1: DatabaseV1 = database.into();
        v1.version = 999;
        let ron_str =
            ron::ser::to_string_pretty(&v1, ron::ser::PrettyConfig::default()).unwrap();
        let err = Database::from_ron(RON_VERSION, &ron_str).unwrap_err();
        let pgerr = err
            .downcast_ref::<PGEnumError>()
            .expect("expected PGEnumError");
        assert!(
            matches!(pgerr, PGEnumError::UnknownVersion(999)),
            "expected UnknownVersion(999), got: {pgerr:?}"
        );
    }

    #[test]
    fn builder_rejects_duplicate_values() {
        let options = SQLParsingOptions::default();
        let mut builder = DatabaseBuilder::new("test", options);
        let err = builder
            .add_enum(
                "public",
                "status",
                vec!["active".into(), "inactive".into(), "active".into()],
                None,
            )
            .unwrap_err();
        let pgerr = err
            .downcast_ref::<PGEnumError>()
            .expect("expected PGEnumError");
        assert!(matches!(
            pgerr,
            PGEnumError::DuplicatedValue {
                schema,
                enum_name,
                value,
            } if schema == "public" && enum_name == "status" && value == "active"
        ));
    }

    #[test]
    fn builder_rejects_duplicate_types() {
        let options = SQLParsingOptions::default();
        let mut builder = DatabaseBuilder::new("test", options);
        builder
            .add_enum("public", "status", vec!["active".into()], None)
            .unwrap();
        let err = builder
            .add_enum("public", "status", vec!["pending".into()], None)
            .unwrap_err();
        let pgerr = err
            .downcast_ref::<PGEnumError>()
            .expect("expected PGEnumError");
        assert!(matches!(
            pgerr,
            PGEnumError::DuplicatedType {
                schema,
                enum_name,
            } if schema == "public" && enum_name == "status"
        ));
    }

    fn build_database_with_conflict() -> Database {
        let mut builder = DatabaseBuilder::new("test", SQLParsingOptions::default());
        builder
            .add_enum("public", "status", vec!["active".into()], None)
            .unwrap();
        builder
            .add_enum("other", "status", vec!["pending".into()], None)
            .unwrap();
        builder
            .add_enum("public", "color", vec!["red".into()], Some("colors"))
            .unwrap();
        builder.into()
    }

    #[test]
    fn get_by_name_returns_unique_enum() {
        let es = build_database_with_conflict();
        let e = es.get(None, "color").unwrap();
        assert_eq!(e.schema, "public");
        assert_eq!(e.name, "color");
        assert_eq!(e.values, vec!["red"]);
        assert_eq!(e.comment.as_deref(), Some("colors"));
    }

    #[test]
    fn get_by_schema_and_name() {
        let es = build_database_with_conflict();
        let e = es.get(Some("public"), "status").unwrap();
        assert_eq!(e.schema, "public");
        assert_eq!(e.values, vec!["active"]);

        let e = es.get(Some("other"), "status").unwrap();
        assert_eq!(e.schema, "other");
        assert_eq!(e.values, vec!["pending"]);
    }

    #[test]
    fn get_without_schema_returns_conflict_for_ambiguous_name() {
        let es = build_database_with_conflict();
        let err = es.get(None, "status").unwrap_err();
        let pgerr = err
            .downcast_ref::<PGEnumError>()
            .expect("expected PGEnumError");
        assert!(matches!(
            pgerr,
            PGEnumError::EnumConflict { name, schemas }
                if name == "status" && schemas.contains(&"public".to_string()) && schemas.contains(&"other".to_string())
        ));
    }

    #[test]
    fn get_returns_not_found_without_schema() {
        let es = build_database_with_conflict();
        let err = es.get(None, "nonexistent").unwrap_err();
        let pgerr = err
            .downcast_ref::<PGEnumError>()
            .expect("expected PGEnumError");
        assert!(matches!(
            pgerr,
            PGEnumError::EnumNotFound { name, schema: None } if name == "nonexistent"
        ));
    }

    #[test]
    fn get_returns_not_found_with_schema() {
        let es = build_database_with_conflict();
        let err = es.get(Some("public"), "nonexistent").unwrap_err();
        let pgerr = err
            .downcast_ref::<PGEnumError>()
            .expect("expected PGEnumError");
        assert!(matches!(
            pgerr,
            PGEnumError::EnumNotFound { name, schema: Some(s) }
                if name == "nonexistent" && s == "public"
        ));
    }

    #[test]
    fn get_returns_not_found_for_wrong_schema() {
        let es = build_database_with_conflict();
        let err = es.get(Some("missing_schema"), "status").unwrap_err();
        let pgerr = err
            .downcast_ref::<PGEnumError>()
            .expect("expected PGEnumError");
        assert!(matches!(
            pgerr,
            PGEnumError::EnumNotFound { name, schema: Some(s) }
                if name == "status" && s == "missing_schema"
        ));
    }

    #[test]
    fn from_sql_parses_enums_with_schema_values_and_comment() {
        let database = Database::from_sql("test", VALID_SQL).unwrap();

        let am = database.enums.iter().find(|e| e.name == "access_management").unwrap();
        assert_eq!(am.schema, "public");
        assert_eq!(am.values.len(), 3);
        assert_eq!(am.values[0], "global");
        assert_eq!(am.values[1], "contextual");
        assert_eq!(am.values[2], "forbidden");
        assert_eq!(
            am.comment.as_deref(),
            Some("Represents access management levels.")
        );
    }

    #[test]
    fn from_sql_parses_enum_without_comment() {
        let database = Database::from_sql("test", VALID_SQL).unwrap();

        let ac = database.enums.iter().find(|e| e.name == "analytics_context").unwrap();
        assert_eq!(ac.schema, "public");
        assert_eq!(ac.values.len(), 2);
        assert!(ac.comment.is_none());
    }

    #[test]
    fn from_sql_parses_enum_without_schema() {
        let database = Database::from_sql("test", VALID_SQL).unwrap();

        let ak = database.enums.iter().find(|e| e.name == "asset_kind").unwrap();
        assert_eq!(ak.schema, "public");
        assert_eq!(ak.values.len(), 7);
    }

    #[test]
    fn from_sql_returns_empty_enums_for_sql_without_enums() {
        let sql = "SELECT 1;";
        let database = Database::from_sql("test", sql).unwrap();
        assert!(database.enums.is_empty());
    }

    #[test]
    fn from_sql_returns_error_for_invalid_sql() {
        let err = Database::from_sql("test", INVALID_SQL).unwrap_err();
        assert!(err.downcast_ref::<PGEnumError>()
            .is_some_and(|e| matches!(e, PGEnumError::SQLParseError(_))));
    }

    #[test]
    fn from_sql_returns_error_for_duplicated_value() {
        let sql = "CREATE TYPE public.status AS ENUM ('active', 'inactive', 'active');";
        let err = Database::from_sql("test", sql).unwrap_err();
        let pgerr = err.downcast_ref::<PGEnumError>().expect("expected PGEnumError");
        assert!(matches!(
            pgerr,
            PGEnumError::DuplicatedValue {
                schema,
                enum_name,
                value,
            } if schema == "public" && enum_name == "status" && value == "active"
        ), "expected DuplicatedValue error, got: {pgerr:?}");
    }

    #[test]
    fn from_sql_returns_error_for_duplicated_type() {
        let sql = "
            CREATE TYPE public.status AS ENUM ('active', 'inactive');
            CREATE TYPE public.status AS ENUM ('pending', 'done');
        ";
        let err = Database::from_sql("test", sql).unwrap_err();
        let pgerr = err.downcast_ref::<PGEnumError>().expect("expected PGEnumError");
        assert!(matches!(
            pgerr,
            PGEnumError::DuplicatedType {
                schema,
                enum_name,
            } if schema == "public" && enum_name == "status"
        ), "expected DuplicatedType error, got: {pgerr:?}");
    }

    #[test]
    fn from_ron_file_reads_fixture() {
        let expected = Database::from_sql("test", VALID_SQL).unwrap();
        let parsed = Database::from_ron_file(RON_VERSION, "tests/fixtures/valid_enums_v1.ron").unwrap();
        assert_eq!(expected, parsed);
    }

    #[test]
    fn from_ron_file_returns_io_error_for_missing_file() {
        let err = Database::from_ron_file(RON_VERSION, "tests/fixtures/nonexistent.ron").unwrap_err();
        assert!(err.downcast_ref::<PGEnumError>()
            .is_some_and(|e| matches!(e, PGEnumError::IOError(_))));
    }

    #[test]
    fn from_sql_file_reads_fixture() {
        let expected = Database::from_sql("test", VALID_SQL).unwrap();
        let parsed = Database::from_sql_file("test", "tests/fixtures/valid_enums.sql").unwrap();
        assert_eq!(expected, parsed);
    }

    #[test]
    fn from_sql_file_returns_io_error_for_missing_file() {
        let err = Database::from_sql_file("test", "tests/fixtures/nonexistent.sql").unwrap_err();
        assert!(err.downcast_ref::<PGEnumError>()
            .is_some_and(|e| matches!(e, PGEnumError::IOError(_))));
    }

    #[test]
    fn to_ron_file_writes_and_reads_back() {
        let database = Database::from_sql("test", VALID_SQL).unwrap();
        let dir = std::env::temp_dir().join("pgenum_parser_tests");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("to_ron_file_test.ron");
        let path_str = path.to_str().unwrap();

        database.to_ron_file(path_str).unwrap();
        let recovered = Database::from_ron_file(RON_VERSION, path_str).unwrap();
        assert_eq!(database, recovered);

        fs::remove_file(&path).ok();
    }

    #[test]
    fn to_ron_file_returns_io_error_for_invalid_path() {
        let database = Database::from_sql("test", VALID_SQL).unwrap();
        let err = database.to_ron_file("/nonexistent_dir/foo/bar.ron").unwrap_err();
        assert!(err.downcast_ref::<PGEnumError>()
            .is_some_and(|e| matches!(e, PGEnumError::IOError(_))));
    }
}
