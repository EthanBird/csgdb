use crate::{
    Database, DatabasePool, Error, ErrorCode, ReadConnection, ReadTransaction, Result, Row,
    Statement, Transaction, Value, ValueRef,
};
use std::collections::HashSet;
use std::fmt;
use std::marker::PhantomData;

const SCHEMA_REGISTRY_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS "__csgdb_schema" (
    "collection_id" TEXT NOT NULL PRIMARY KEY,
    "table_name" TEXT NOT NULL UNIQUE,
    "schema_version" INTEGER NOT NULL CHECK ("schema_version" > 0),
    "fingerprint" BLOB NOT NULL CHECK (length("fingerprint") = 32),
    "descriptor" TEXT NOT NULL
)
"#;
const FIND_REGISTERED_SCHEMA_SQL: &str = r#"
SELECT "table_name", "schema_version", "fingerprint", "descriptor"
FROM "__csgdb_schema"
WHERE "collection_id" = ?1
"#;
const INSERT_REGISTERED_SCHEMA_SQL: &str = r#"
INSERT INTO "__csgdb_schema" (
    "collection_id", "table_name", "schema_version", "fingerprint", "descriptor"
) VALUES (?1, ?2, ?3, ?4, ?5)
"#;
const FIND_TABLE_OWNER_SQL: &str = r#"
SELECT count(*)
FROM "__csgdb_schema"
WHERE "table_name" = ?1 AND "collection_id" <> ?2
"#;
const TABLE_COLUMNS_SQL: &str = r#"
SELECT "name", "type", "notnull", "pk"
FROM pragma_table_info(?1)
ORDER BY "name"
"#;
const INDEX_REGISTRY_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS "__csgdb_index_schema" (
    "collection_id" TEXT NOT NULL,
    "index_id" TEXT NOT NULL,
    "index_name" TEXT NOT NULL UNIQUE,
    "is_unique" INTEGER NOT NULL CHECK ("is_unique" IN (0, 1)),
    "fingerprint" BLOB NOT NULL CHECK (length("fingerprint") = 32),
    "descriptor" TEXT NOT NULL,
    PRIMARY KEY ("collection_id", "index_id"),
    FOREIGN KEY ("collection_id") REFERENCES "__csgdb_schema" ("collection_id")
)
"#;
const MIGRATION_REGISTRY_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS "__csgdb_migration" (
    "collection_id" TEXT NOT NULL,
    "migration_id" TEXT NOT NULL,
    "from_version" INTEGER NOT NULL CHECK ("from_version" > 0),
    "to_version" INTEGER NOT NULL CHECK ("to_version" > "from_version"),
    "from_fingerprint" BLOB NOT NULL CHECK (length("from_fingerprint") = 32),
    "to_fingerprint" BLOB NOT NULL CHECK (length("to_fingerprint") = 32),
    PRIMARY KEY ("collection_id", "migration_id"),
    FOREIGN KEY ("collection_id") REFERENCES "__csgdb_schema" ("collection_id")
)
"#;
const FIND_REGISTERED_INDEXES_SQL: &str = r#"
SELECT "index_id", "index_name", "is_unique", "fingerprint", "descriptor"
FROM "__csgdb_index_schema"
WHERE "collection_id" = ?1
ORDER BY "index_id"
"#;
const INSERT_REGISTERED_INDEX_SQL: &str = r#"
INSERT INTO "__csgdb_index_schema" (
    "collection_id", "index_id", "index_name", "is_unique", "fingerprint", "descriptor"
) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
"#;
const DELETE_REGISTERED_INDEXES_SQL: &str =
    "DELETE FROM \"__csgdb_index_schema\" WHERE \"collection_id\" = ?1";
const UPDATE_REGISTERED_SCHEMA_SQL: &str = r#"
UPDATE "__csgdb_schema"
SET "table_name" = ?2, "schema_version" = ?3, "fingerprint" = ?4, "descriptor" = ?5
WHERE "collection_id" = ?1
"#;
const FIND_MIGRATION_SQL: &str = r#"
SELECT "from_version", "to_version", "from_fingerprint", "to_fingerprint"
FROM "__csgdb_migration"
WHERE "collection_id" = ?1 AND "migration_id" = ?2
"#;
const INSERT_MIGRATION_SQL: &str = r#"
INSERT INTO "__csgdb_migration" (
    "collection_id", "migration_id", "from_version", "to_version",
    "from_fingerprint", "to_fingerprint"
) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
"#;
const PHYSICAL_INDEX_SQL: &str = r#"
SELECT "unique", "origin", "partial"
FROM pragma_index_list(?1)
WHERE "name" = ?2
"#;
const PHYSICAL_INDEX_COLUMNS_SQL: &str = r#"
SELECT coalesce("name", ''), "desc", coalesce("coll", ''), "key"
FROM pragma_index_xinfo(?1)
ORDER BY "seqno"
"#;

/// A storage class accepted by a typed collection field.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum ColumnType {
    Integer,
    Real,
    Text,
    Blob,
}

impl ColumnType {
    #[must_use]
    pub const fn sql_name(self) -> &'static str {
        match self {
            Self::Integer => "INTEGER",
            Self::Real => "REAL",
            Self::Text => "TEXT",
            Self::Blob => "BLOB",
        }
    }
}

/// Stable metadata for one field in a collection.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FieldSchema {
    id: &'static str,
    column: &'static str,
    column_type: ColumnType,
    nullable: bool,
    primary_key: bool,
}

impl FieldSchema {
    #[doc(hidden)]
    #[must_use]
    pub const fn new(
        id: &'static str,
        column: &'static str,
        column_type: ColumnType,
        nullable: bool,
        primary_key: bool,
    ) -> Self {
        Self {
            id,
            column,
            column_type,
            nullable,
            primary_key,
        }
    }

    #[must_use]
    pub const fn id(&self) -> &'static str {
        self.id
    }

    #[must_use]
    pub const fn column(&self) -> &'static str {
        self.column
    }

    #[must_use]
    pub const fn column_type(&self) -> ColumnType {
        self.column_type
    }

    #[must_use]
    pub const fn is_nullable(&self) -> bool {
        self.nullable
    }

    #[must_use]
    pub const fn is_primary_key(&self) -> bool {
        self.primary_key
    }
}

/// A small, copyable typed handle to one field of a collection.
///
/// Derive-generated handles such as `Memory::FIELD_TEXT` are the stable input
/// boundary for typed query construction. The Rust constant name may change
/// during a source refactor; [`CollectionField::id`] remains the persisted
/// identity used by schema metadata and future query plans.
pub struct CollectionField<C, T> {
    schema: &'static FieldSchema,
    marker: PhantomData<fn() -> (C, T)>,
}

impl<C, T> CollectionField<C, T> {
    #[doc(hidden)]
    #[must_use]
    pub const fn new(schema: &'static FieldSchema) -> Self {
        Self {
            schema,
            marker: PhantomData,
        }
    }

    #[must_use]
    pub const fn id(&self) -> &'static str {
        self.schema.id()
    }

    #[must_use]
    pub const fn column(&self) -> &'static str {
        self.schema.column()
    }

    #[must_use]
    pub const fn column_type(&self) -> ColumnType {
        self.schema.column_type()
    }

    #[must_use]
    pub const fn is_nullable(&self) -> bool {
        self.schema.is_nullable()
    }

    #[must_use]
    pub const fn is_primary_key(&self) -> bool {
        self.schema.is_primary_key()
    }

    #[must_use]
    pub const fn schema(&self) -> &'static FieldSchema {
        self.schema
    }
}

impl<C: Collection, T: FieldValue> CollectionField<C, T> {
    /// Returns the owning collection schema.
    #[must_use]
    pub fn collection(&self) -> &'static CollectionSchema {
        C::schema()
    }

    /// Encodes a value using this field's statically known Rust type.
    ///
    /// # Errors
    ///
    /// Returns an error when the value cannot be represented by the database
    /// storage class.
    pub fn encode(&self, value: &T) -> Result<Value> {
        value.to_value()
    }

    /// Tests persistent identity across Rust record types and field types.
    ///
    /// This is useful when checking whether a source-level rename still points
    /// to the same registered collection field.
    #[must_use]
    pub fn same_storage_field<OtherCollection, OtherValue>(
        &self,
        other: &CollectionField<OtherCollection, OtherValue>,
    ) -> bool
    where
        OtherCollection: Collection,
        OtherValue: FieldValue,
    {
        C::schema().id() == OtherCollection::schema().id() && self.id() == other.id()
    }
}

impl<C, T> Copy for CollectionField<C, T> {}

impl<C, T> Clone for CollectionField<C, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<C, T> fmt::Debug for CollectionField<C, T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CollectionField")
            .field("id", &self.id())
            .field("column", &self.column())
            .field("column_type", &self.column_type())
            .field("nullable", &self.is_nullable())
            .field("primary_key", &self.is_primary_key())
            .finish()
    }
}

/// SHA-256 digest of a canonical, order-independent collection descriptor.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct SchemaFingerprint([u8; 32]);

impl SchemaFingerprint {
    #[doc(hidden)]
    #[must_use]
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for SchemaFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "SchemaFingerprint({self})")
    }
}

impl fmt::Display for SchemaFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Stable metadata for one declared collection index.
///
/// Index fingerprints are independent from the base collection fingerprint
/// because indexes are derived structures that can be rebuilt transactionally.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct IndexSchema {
    id: &'static str,
    name: &'static str,
    columns: &'static [&'static str],
    unique: bool,
    fingerprint: SchemaFingerprint,
    canonical_descriptor: &'static str,
}

impl IndexSchema {
    #[doc(hidden)]
    #[must_use]
    pub const fn new(
        id: &'static str,
        name: &'static str,
        columns: &'static [&'static str],
        unique: bool,
        fingerprint: SchemaFingerprint,
        canonical_descriptor: &'static str,
    ) -> Self {
        Self {
            id,
            name,
            columns,
            unique,
            fingerprint,
            canonical_descriptor,
        }
    }

    #[must_use]
    pub const fn id(&self) -> &'static str {
        self.id
    }

    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }

    #[must_use]
    pub const fn columns(&self) -> &'static [&'static str] {
        self.columns
    }

    #[must_use]
    pub const fn is_unique(&self) -> bool {
        self.unique
    }

    #[must_use]
    pub const fn fingerprint(&self) -> SchemaFingerprint {
        self.fingerprint
    }

    #[must_use]
    pub const fn canonical_descriptor(&self) -> &'static str {
        self.canonical_descriptor
    }
}

/// Stable collection metadata generated by `#[derive(Collection)]`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CollectionSchema {
    id: &'static str,
    table: &'static str,
    version: u32,
    fingerprint: SchemaFingerprint,
    fields: &'static [FieldSchema],
    canonical_descriptor: &'static str,
    indexes: &'static [IndexSchema],
}

impl CollectionSchema {
    #[doc(hidden)]
    #[must_use]
    pub const fn new(
        id: &'static str,
        table: &'static str,
        version: u32,
        fingerprint: SchemaFingerprint,
        fields: &'static [FieldSchema],
        canonical_descriptor: &'static str,
        indexes: &'static [IndexSchema],
    ) -> Self {
        Self {
            id,
            table,
            version,
            fingerprint,
            fields,
            canonical_descriptor,
            indexes,
        }
    }

    #[must_use]
    pub const fn id(&self) -> &'static str {
        self.id
    }

    #[must_use]
    pub const fn table(&self) -> &'static str {
        self.table
    }

    #[must_use]
    pub const fn version(&self) -> u32 {
        self.version
    }

    #[must_use]
    pub const fn fingerprint(&self) -> SchemaFingerprint {
        self.fingerprint
    }

    #[must_use]
    pub const fn fields(&self) -> &'static [FieldSchema] {
        self.fields
    }

    #[must_use]
    pub const fn canonical_descriptor(&self) -> &'static str {
        self.canonical_descriptor
    }

    #[must_use]
    pub const fn indexes(&self) -> &'static [IndexSchema] {
        self.indexes
    }

    #[must_use]
    pub fn primary_key(&self) -> Option<&'static FieldSchema> {
        self.fields.iter().find(|field| field.primary_key)
    }
}

/// Persisted schema metadata read from the database registry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegisteredSchema {
    collection_id: String,
    table_name: String,
    version: u32,
    fingerprint: SchemaFingerprint,
    canonical_descriptor: String,
}

impl RegisteredSchema {
    #[must_use]
    pub fn collection_id(&self) -> &str {
        &self.collection_id
    }

    #[must_use]
    pub fn table_name(&self) -> &str {
        &self.table_name
    }

    #[must_use]
    pub const fn version(&self) -> u32 {
        self.version
    }

    #[must_use]
    pub const fn fingerprint(&self) -> SchemaFingerprint {
        self.fingerprint
    }

    #[must_use]
    pub fn canonical_descriptor(&self) -> &str {
        &self.canonical_descriptor
    }
}

/// Outcome of registering a typed collection.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum SchemaRegistration {
    Created,
    AlreadyRegistered,
}

/// Outcome of applying an explicitly identified collection migration.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum MigrationStatus {
    Applied,
    AlreadyApplied,
}

/// Conversion boundary between Rust field types and database values.
pub trait FieldValue: Sized {
    const COLUMN_TYPE: ColumnType;
    const NULLABLE: bool = false;

    /// Converts this field to an owned database value.
    ///
    /// # Errors
    ///
    /// Returns an error when the Rust value cannot be represented by the
    /// database storage class.
    fn to_value(&self) -> Result<Value>;

    /// Decodes one borrowed database value without cross-class coercion.
    ///
    /// # Errors
    ///
    /// Returns an error for an incompatible storage class or an out-of-range
    /// numeric value.
    fn from_value(value: ValueRef<'_>) -> Result<Self>;
}

macro_rules! signed_integer_field {
    ($($type:ty),+ $(,)?) => {
        $(
            impl FieldValue for $type {
                const COLUMN_TYPE: ColumnType = ColumnType::Integer;

                fn to_value(&self) -> Result<Value> {
                    Ok(Value::Integer(i64::from(*self)))
                }

                fn from_value(value: ValueRef<'_>) -> Result<Self> {
                    match value {
                        ValueRef::Integer(value) => Self::try_from(value)
                            .map_err(|_| invalid_field_value()),
                        _ => Err(invalid_field_type()),
                    }
                }
            }
        )+
    };
}

macro_rules! unsigned_integer_field {
    ($($type:ty),+ $(,)?) => {
        $(
            impl FieldValue for $type {
                const COLUMN_TYPE: ColumnType = ColumnType::Integer;

                fn to_value(&self) -> Result<Value> {
                    i64::try_from(*self)
                        .map(Value::Integer)
                        .map_err(|_| invalid_field_value())
                }

                fn from_value(value: ValueRef<'_>) -> Result<Self> {
                    match value {
                        ValueRef::Integer(value) => Self::try_from(value)
                            .map_err(|_| invalid_field_value()),
                        _ => Err(invalid_field_type()),
                    }
                }
            }
        )+
    };
}

signed_integer_field!(i8, i16, i32, i64);
unsigned_integer_field!(u8, u16, u32, u64);

impl FieldValue for bool {
    const COLUMN_TYPE: ColumnType = ColumnType::Integer;

    fn to_value(&self) -> Result<Value> {
        Ok(Value::Integer(i64::from(*self)))
    }

    fn from_value(value: ValueRef<'_>) -> Result<Self> {
        match value {
            ValueRef::Integer(0) => Ok(false),
            ValueRef::Integer(1) => Ok(true),
            ValueRef::Integer(_) => Err(invalid_field_value()),
            _ => Err(invalid_field_type()),
        }
    }
}

impl FieldValue for f64 {
    const COLUMN_TYPE: ColumnType = ColumnType::Real;

    fn to_value(&self) -> Result<Value> {
        Ok(Value::Real(*self))
    }

    fn from_value(value: ValueRef<'_>) -> Result<Self> {
        match value {
            ValueRef::Real(value) => Ok(value),
            _ => Err(invalid_field_type()),
        }
    }
}

impl FieldValue for f32 {
    const COLUMN_TYPE: ColumnType = ColumnType::Real;

    fn to_value(&self) -> Result<Value> {
        Ok(Value::Real(f64::from(*self)))
    }

    #[allow(clippy::cast_possible_truncation)]
    fn from_value(value: ValueRef<'_>) -> Result<Self> {
        match value {
            ValueRef::Real(value) => {
                let narrowed = value as Self;
                if value.is_finite() && !narrowed.is_finite() {
                    Err(invalid_field_value())
                } else {
                    Ok(narrowed)
                }
            }
            _ => Err(invalid_field_type()),
        }
    }
}

impl FieldValue for String {
    const COLUMN_TYPE: ColumnType = ColumnType::Text;

    fn to_value(&self) -> Result<Value> {
        Ok(Value::Text(self.clone()))
    }

    fn from_value(value: ValueRef<'_>) -> Result<Self> {
        match value {
            ValueRef::Text(value) => Ok(value.to_owned()),
            _ => Err(invalid_field_type()),
        }
    }
}

impl FieldValue for Vec<u8> {
    const COLUMN_TYPE: ColumnType = ColumnType::Blob;

    fn to_value(&self) -> Result<Value> {
        Ok(Value::Blob(self.clone()))
    }

    fn from_value(value: ValueRef<'_>) -> Result<Self> {
        match value {
            ValueRef::Blob(value) => Ok(value.to_owned()),
            _ => Err(invalid_field_type()),
        }
    }
}

impl<T: FieldValue> FieldValue for Option<T> {
    const COLUMN_TYPE: ColumnType = T::COLUMN_TYPE;
    const NULLABLE: bool = true;

    fn to_value(&self) -> Result<Value> {
        self.as_ref().map_or(Ok(Value::Null), FieldValue::to_value)
    }

    fn from_value(value: ValueRef<'_>) -> Result<Self> {
        match value {
            ValueRef::Null => Ok(None),
            value => T::from_value(value).map(Some),
        }
    }
}

/// A record with an explicit, stable database schema.
///
/// Implementations are normally generated with `#[derive(Collection)]`.
pub trait Collection: Sized {
    type Key: FieldValue;

    #[doc(hidden)]
    const CREATE_TABLE_SQL: &'static str;
    #[doc(hidden)]
    const INSERT_SQL: &'static str;
    #[doc(hidden)]
    const SELECT_BY_KEY_SQL: &'static str;
    #[doc(hidden)]
    const UPDATE_SQL: &'static str;
    #[doc(hidden)]
    const DELETE_BY_KEY_SQL: &'static str;
    #[doc(hidden)]
    const CREATE_INDEX_SQL: &'static [&'static str];

    /// Starts a typed query draft for this collection.
    ///
    /// The draft cannot execute until a mandatory result bound is supplied
    /// with [`crate::QueryDraft::take`].
    #[must_use]
    fn query() -> crate::QueryDraft<Self> {
        crate::QueryDraft::new()
    }

    fn schema() -> &'static CollectionSchema;

    #[doc(hidden)]
    fn values(&self) -> Result<Vec<Value>>;

    #[doc(hidden)]
    fn key_value(&self) -> Result<Value>;

    #[doc(hidden)]
    fn from_row(row: &Row<'_>) -> Result<Self>;
}

/// Typed CRUD operations available inside an explicit transaction.
pub trait CollectionCrud {
    /// Inserts one record.
    ///
    /// # Errors
    ///
    /// Returns an error for value conversion, binding, constraint, or storage
    /// failures.
    fn insert<C: Collection>(&self, record: &C) -> Result<usize>;

    /// Gets one record by its typed primary key.
    ///
    /// # Errors
    ///
    /// Returns an error for key conversion, decoding, or storage failures.
    fn get<C: Collection>(&self, key: &C::Key) -> Result<Option<C>>;

    /// Updates every non-key field of one record.
    ///
    /// # Errors
    ///
    /// Returns an error for value conversion, binding, constraint, or storage
    /// failures.
    fn update<C: Collection>(&self, record: &C) -> Result<usize>;

    /// Deletes one record by its typed primary key.
    ///
    /// # Errors
    ///
    /// Returns an error for key conversion, binding, or storage failures.
    fn delete<C: Collection>(&self, key: &C::Key) -> Result<usize>;
}

impl CollectionCrud for Database {
    fn insert<C: Collection>(&self, record: &C) -> Result<usize> {
        let values = record.values()?;
        execute_record(self.prepare_cached(C::INSERT_SQL)?, &values)
    }

    fn get<C: Collection>(&self, key: &C::Key) -> Result<Option<C>> {
        let key = key.to_value()?;
        query_record(self.prepare_cached(C::SELECT_BY_KEY_SQL)?, &key)
    }

    fn update<C: Collection>(&self, record: &C) -> Result<usize> {
        let values = update_values(record)?;
        execute_record(self.prepare_cached(C::UPDATE_SQL)?, &values)
    }

    fn delete<C: Collection>(&self, key: &C::Key) -> Result<usize> {
        let values = [key.to_value()?];
        execute_record(self.prepare_cached(C::DELETE_BY_KEY_SQL)?, &values)
    }
}

impl CollectionCrud for Transaction<'_> {
    fn insert<C: Collection>(&self, record: &C) -> Result<usize> {
        let values = record.values()?;
        execute_record(self.prepare(C::INSERT_SQL)?, &values)
    }

    fn get<C: Collection>(&self, key: &C::Key) -> Result<Option<C>> {
        let key = key.to_value()?;
        query_record(self.prepare(C::SELECT_BY_KEY_SQL)?, &key)
    }

    fn update<C: Collection>(&self, record: &C) -> Result<usize> {
        let values = update_values(record)?;
        execute_record(self.prepare(C::UPDATE_SQL)?, &values)
    }

    fn delete<C: Collection>(&self, key: &C::Key) -> Result<usize> {
        let values = [key.to_value()?];
        execute_record(self.prepare(C::DELETE_BY_KEY_SQL)?, &values)
    }
}

impl Database {
    /// Creates or validates a typed collection and persists its fingerprint.
    ///
    /// The table and registry row are changed atomically. A previously
    /// registered incompatible fingerprint is never overwritten.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::SchemaMismatch`] for incompatible registered or
    /// physical schemas, or a storage error when registration cannot commit.
    pub fn register_collection<C: Collection>(&mut self) -> Result<SchemaRegistration> {
        let schema = C::schema();
        validate_collection_schema(schema)?;
        let transaction = self.transaction()?;
        initialize_schema_registries(&transaction)?;
        let existing = find_registered_schema(&transaction, schema.id())?;
        let registration = if let Some(existing) = existing {
            ensure_registered_schema_matches(schema, &existing)?;
            validate_registered_indexes(&transaction, schema)?;
            SchemaRegistration::AlreadyRegistered
        } else {
            ensure_table_is_unclaimed(&transaction, schema)?;
            transaction.execute_batch(C::CREATE_TABLE_SQL)?;
            insert_registered_schema(&transaction, schema)?;
            create_declared_indexes::<C>(&transaction)?;
            insert_registered_indexes(&transaction, schema)?;
            SchemaRegistration::Created
        };
        validate_physical_table(&transaction, schema)?;
        validate_physical_indexes(&transaction, schema)?;
        transaction.commit()?;
        Ok(registration)
    }

    /// Applies one explicit, transactional collection schema migration.
    ///
    /// `From` and `To` must share a stable collection ID, and `To` must have a
    /// greater version and a different base fingerprint. The callback performs
    /// the application-specific table/data transformation. Registry and index
    /// updates occur only after the target physical schema validates.
    ///
    /// A successfully recorded migration is idempotent: applying the same ID
    /// and endpoints again returns [`MigrationStatus::AlreadyApplied`] without
    /// invoking the callback.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::InvalidMigration`] for invalid endpoints,
    /// [`ErrorCode::MigrationMismatch`] when an ID or current state conflicts,
    /// a schema mismatch when either physical endpoint is incompatible, or an
    /// error returned by the callback. Every failure rolls back the transaction.
    pub fn migrate_collection<From, To, F>(
        &mut self,
        migration_id: &str,
        operation: F,
    ) -> Result<MigrationStatus>
    where
        From: Collection,
        To: Collection,
        F: FnOnce(&Transaction<'_>) -> Result<()>,
    {
        let from = From::schema();
        let to = To::schema();
        validate_migration(from, to, migration_id)?;
        let transaction = self.transaction()?;
        initialize_schema_registries(&transaction)?;

        if let Some(migration) = find_migration(&transaction, from.id(), migration_id)? {
            ensure_migration_matches(&migration, from, to)?;
            let current =
                find_registered_schema(&transaction, from.id())?.ok_or_else(migration_mismatch)?;
            ensure_registered_schema_matches(to, &current).map_err(|_| migration_mismatch())?;
            validate_registered_indexes(&transaction, to).map_err(|_| migration_mismatch())?;
            validate_physical_table(&transaction, to).map_err(|_| migration_mismatch())?;
            validate_physical_indexes(&transaction, to).map_err(|_| migration_mismatch())?;
            transaction.commit()?;
            return Ok(MigrationStatus::AlreadyApplied);
        }

        let current =
            find_registered_schema(&transaction, from.id())?.ok_or_else(migration_mismatch)?;
        ensure_registered_schema_matches(from, &current).map_err(|_| migration_mismatch())?;
        validate_registered_indexes(&transaction, from).map_err(|_| migration_mismatch())?;
        validate_physical_table(&transaction, from)?;
        validate_physical_indexes(&transaction, from)?;

        operation(&transaction)?;
        ensure_table_is_unclaimed(&transaction, to)?;
        validate_physical_table(&transaction, to)?;
        replace_declared_indexes::<From, To>(&transaction)?;
        validate_physical_indexes(&transaction, to)?;
        update_registered_schema(&transaction, to)?;
        replace_registered_indexes(&transaction, to)?;
        insert_migration(&transaction, migration_id, from, to)?;
        transaction.commit()?;
        Ok(MigrationStatus::Applied)
    }

    /// Reads persisted metadata for one stable collection ID.
    ///
    /// # Errors
    ///
    /// Returns an error when the registry is unavailable or malformed.
    pub fn registered_schema(&self, collection_id: &str) -> Result<Option<RegisteredSchema>> {
        if !valid_name(collection_id) {
            return Err(invalid_schema());
        }
        if !schema_registry_exists(self)? {
            return Ok(None);
        }
        let statement = self.prepare_cached(FIND_REGISTERED_SCHEMA_SQL)?;
        query_registered_schema(statement, collection_id)
    }

    /// Inserts one typed record.
    ///
    /// # Errors
    ///
    /// Returns an error for conversion, constraint, or storage failures.
    pub fn insert<C: Collection>(&self, record: &C) -> Result<usize> {
        CollectionCrud::insert(self, record)
    }

    /// Gets one typed record by primary key.
    ///
    /// # Errors
    ///
    /// Returns an error for conversion, decoding, or storage failures.
    pub fn get<C: Collection>(&self, key: &C::Key) -> Result<Option<C>> {
        CollectionCrud::get::<C>(self, key)
    }

    /// Updates every non-key field of one typed record.
    ///
    /// # Errors
    ///
    /// Returns an error for conversion, constraint, or storage failures.
    pub fn update<C: Collection>(&self, record: &C) -> Result<usize> {
        CollectionCrud::update(self, record)
    }

    /// Deletes one typed record by primary key.
    ///
    /// # Errors
    ///
    /// Returns an error for conversion, binding, or storage failures.
    pub fn delete<C: Collection>(&self, key: &C::Key) -> Result<usize> {
        CollectionCrud::delete::<C>(self, key)
    }
}

impl DatabasePool {
    /// Creates or validates a collection on the serialized writer.
    ///
    /// # Errors
    ///
    /// Returns a schema, queue, transaction, or storage error.
    pub fn register_collection<C: Collection + 'static>(&self) -> Result<SchemaRegistration> {
        self.write(Database::register_collection::<C>)
    }

    /// Applies an explicit collection migration on the serialized writer.
    ///
    /// This is an exclusive write callback and therefore forms a barrier with
    /// bounded group-commit jobs.
    ///
    /// # Errors
    ///
    /// Returns any migration, queue, callback, transaction, or storage error.
    pub fn migrate_collection<From, To, F>(
        &self,
        migration_id: impl Into<String>,
        operation: F,
    ) -> Result<MigrationStatus>
    where
        From: Collection + 'static,
        To: Collection + 'static,
        F: FnOnce(&Transaction<'_>) -> Result<()> + Send + 'static,
    {
        let migration_id = migration_id.into();
        self.write(move |database| {
            database.migrate_collection::<From, To, _>(&migration_id, operation)
        })
    }

    /// Reads persisted metadata using a pooled read connection.
    ///
    /// # Errors
    ///
    /// Returns an error when checkout, querying, or decoding fails.
    pub fn registered_schema(&self, collection_id: &str) -> Result<Option<RegisteredSchema>> {
        if !valid_name(collection_id) {
            return Err(invalid_schema());
        }
        self.read(|connection| {
            if !schema_registry_exists_on_reader(connection)? {
                return Ok(None);
            }
            let statement = connection.prepare_cached(FIND_REGISTERED_SCHEMA_SQL)?;
            query_registered_schema(statement, collection_id)
        })
    }

    /// Queues one typed insert. It remains eligible for bounded group commit.
    ///
    /// # Errors
    ///
    /// Returns an error for conversion, queue, constraint, or storage failures.
    pub fn insert<C: Collection>(&self, record: &C) -> Result<usize> {
        self.execute(C::INSERT_SQL, record.values()?)
    }

    /// Gets one typed record using a pooled read-only connection.
    ///
    /// # Errors
    ///
    /// Returns an error for checkout, conversion, decoding, or storage failures.
    pub fn get<C: Collection>(&self, key: &C::Key) -> Result<Option<C>> {
        let value = key.to_value()?;
        self.read(|connection| {
            query_record(connection.prepare_cached(C::SELECT_BY_KEY_SQL)?, &value)
        })
    }

    /// Queues one typed update. It remains eligible for bounded group commit.
    ///
    /// # Errors
    ///
    /// Returns an error for conversion, queue, constraint, or storage failures.
    pub fn update<C: Collection>(&self, record: &C) -> Result<usize> {
        self.execute(C::UPDATE_SQL, update_values(record)?)
    }

    /// Queues one typed delete. It remains eligible for bounded group commit.
    ///
    /// # Errors
    ///
    /// Returns an error for conversion, queue, or storage failures.
    pub fn delete<C: Collection>(&self, key: &C::Key) -> Result<usize> {
        self.execute(C::DELETE_BY_KEY_SQL, vec![key.to_value()?])
    }
}

impl ReadConnection<'_> {
    /// Gets one typed record from this pooled read-only connection.
    ///
    /// # Errors
    ///
    /// Returns an error for conversion, decoding, or storage failures.
    pub fn get<C: Collection>(&self, key: &C::Key) -> Result<Option<C>> {
        let key = key.to_value()?;
        query_record(self.prepare_cached(C::SELECT_BY_KEY_SQL)?, &key)
    }
}

impl ReadTransaction<'_> {
    /// Gets one typed record from the current read snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error for conversion, decoding, or storage failures.
    pub fn get<C: Collection>(&self, key: &C::Key) -> Result<Option<C>> {
        let key = key.to_value()?;
        query_record(self.prepare(C::SELECT_BY_KEY_SQL)?, &key)
    }
}

fn execute_record(mut statement: Statement<'_>, values: &[Value]) -> Result<usize> {
    let references = values.iter().map(Value::as_ref).collect::<Vec<_>>();
    statement.execute(&references)
}

fn query_record<C: Collection>(mut statement: Statement<'_>, key: &Value) -> Result<Option<C>> {
    let mut rows = statement.query(&[key.as_ref()])?;
    rows.next_row()?.map(|row| C::from_row(&row)).transpose()
}

fn update_values<C: Collection>(record: &C) -> Result<Vec<Value>> {
    let mut values = record.values()?;
    let primary_key_index = C::schema()
        .fields()
        .iter()
        .position(FieldSchema::is_primary_key)
        .ok_or_else(invalid_schema)?;
    if primary_key_index >= values.len() {
        return Err(invalid_schema());
    }
    let key = values.remove(primary_key_index);
    values.push(key);
    Ok(values)
}

fn initialize_schema_registries(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute_batch(SCHEMA_REGISTRY_SQL)?;
    transaction.execute_batch(INDEX_REGISTRY_SQL)?;
    transaction.execute_batch(MIGRATION_REGISTRY_SQL)
}

fn find_registered_schema(
    transaction: &Transaction<'_>,
    collection_id: &str,
) -> Result<Option<RegisteredSchema>> {
    let statement = transaction.prepare(FIND_REGISTERED_SCHEMA_SQL)?;
    query_registered_schema(statement, collection_id)
}

fn query_registered_schema(
    mut statement: Statement<'_>,
    collection_id: &str,
) -> Result<Option<RegisteredSchema>> {
    let mut rows = statement.query(&[ValueRef::Text(collection_id)])?;
    let Some(row) = rows.next_row()? else {
        return Ok(None);
    };
    let table_name = row.get_text(0)?.to_owned();
    let version = u32::try_from(row.get_i64(1)?).map_err(|_| schema_mismatch())?;
    let fingerprint = fingerprint_from_slice(row.get_blob(2)?)?;
    let canonical_descriptor = row.get_text(3)?.to_owned();
    if !valid_name(&table_name) || version == 0 || canonical_descriptor.is_empty() {
        return Err(schema_mismatch());
    }
    Ok(Some(RegisteredSchema {
        collection_id: collection_id.to_owned(),
        table_name,
        version,
        fingerprint,
        canonical_descriptor,
    }))
}

fn insert_registered_schema(
    transaction: &Transaction<'_>,
    schema: &CollectionSchema,
) -> Result<()> {
    let version = i64::from(schema.version());
    transaction.execute(
        INSERT_REGISTERED_SCHEMA_SQL,
        &[
            ValueRef::Text(schema.id()),
            ValueRef::Text(schema.table()),
            ValueRef::Integer(version),
            ValueRef::Blob(schema.fingerprint().as_bytes()),
            ValueRef::Text(schema.canonical_descriptor()),
        ],
    )?;
    Ok(())
}

fn update_registered_schema(
    transaction: &Transaction<'_>,
    schema: &CollectionSchema,
) -> Result<()> {
    let changed = transaction.execute(
        UPDATE_REGISTERED_SCHEMA_SQL,
        &[
            ValueRef::Text(schema.id()),
            ValueRef::Text(schema.table()),
            ValueRef::Integer(i64::from(schema.version())),
            ValueRef::Blob(schema.fingerprint().as_bytes()),
            ValueRef::Text(schema.canonical_descriptor()),
        ],
    )?;
    if changed == 1 {
        Ok(())
    } else {
        Err(migration_mismatch())
    }
}

#[derive(Debug, Eq, PartialEq)]
struct RegisteredIndex {
    id: String,
    name: String,
    unique: bool,
    fingerprint: SchemaFingerprint,
    descriptor: String,
}

fn registered_indexes(
    transaction: &Transaction<'_>,
    collection_id: &str,
) -> Result<Vec<RegisteredIndex>> {
    let mut statement = transaction.prepare(FIND_REGISTERED_INDEXES_SQL)?;
    let mut rows = statement.query(&[ValueRef::Text(collection_id)])?;
    let mut indexes = Vec::new();
    while let Some(row) = rows.next_row()? {
        let unique = match row.get_i64(2)? {
            0 => false,
            1 => true,
            _ => return Err(schema_mismatch()),
        };
        let index = RegisteredIndex {
            id: row.get_text(0)?.to_owned(),
            name: row.get_text(1)?.to_owned(),
            unique,
            fingerprint: fingerprint_from_slice(row.get_blob(3)?)?,
            descriptor: row.get_text(4)?.to_owned(),
        };
        if !valid_name(&index.id)
            || !valid_name(&index.name)
            || index.name.starts_with("__csgdb_")
            || index.descriptor.is_empty()
        {
            return Err(schema_mismatch());
        }
        indexes.push(index);
    }
    Ok(indexes)
}

fn validate_registered_indexes(
    transaction: &Transaction<'_>,
    schema: &CollectionSchema,
) -> Result<()> {
    let actual = registered_indexes(transaction, schema.id())?;
    let mut expected = schema.indexes().iter().collect::<Vec<_>>();
    expected.sort_unstable_by(|left, right| left.id().cmp(right.id()));
    if actual.len() != expected.len() {
        return Err(schema_mismatch());
    }
    for (actual, expected) in actual.iter().zip(expected) {
        if actual.id != expected.id()
            || actual.name != expected.name()
            || actual.unique != expected.is_unique()
            || actual.fingerprint != expected.fingerprint()
            || actual.descriptor != expected.canonical_descriptor()
        {
            return Err(schema_mismatch());
        }
    }
    Ok(())
}

fn insert_registered_indexes(
    transaction: &Transaction<'_>,
    schema: &CollectionSchema,
) -> Result<()> {
    for index in schema.indexes() {
        transaction.execute(
            INSERT_REGISTERED_INDEX_SQL,
            &[
                ValueRef::Text(schema.id()),
                ValueRef::Text(index.id()),
                ValueRef::Text(index.name()),
                ValueRef::Integer(i64::from(index.is_unique())),
                ValueRef::Blob(index.fingerprint().as_bytes()),
                ValueRef::Text(index.canonical_descriptor()),
            ],
        )?;
    }
    Ok(())
}

fn replace_registered_indexes(
    transaction: &Transaction<'_>,
    schema: &CollectionSchema,
) -> Result<()> {
    transaction.execute(
        DELETE_REGISTERED_INDEXES_SQL,
        &[ValueRef::Text(schema.id())],
    )?;
    insert_registered_indexes(transaction, schema)
}

fn create_declared_indexes<C: Collection>(transaction: &Transaction<'_>) -> Result<()> {
    if C::CREATE_INDEX_SQL.len() != C::schema().indexes().len() {
        return Err(invalid_schema());
    }
    for sql in C::CREATE_INDEX_SQL {
        transaction.execute_batch(sql)?;
    }
    Ok(())
}

fn replace_declared_indexes<From: Collection, To: Collection>(
    transaction: &Transaction<'_>,
) -> Result<()> {
    for old in From::schema().indexes() {
        let retained = To::schema().indexes().iter().any(|new| {
            old.id() == new.id()
                && old.name() == new.name()
                && old.fingerprint() == new.fingerprint()
                && old.canonical_descriptor() == new.canonical_descriptor()
        });
        if !retained {
            transaction.execute_batch(&format!(
                "DROP INDEX IF EXISTS {}",
                quote_sql_identifier(old.name())
            ))?;
        }
    }
    create_declared_indexes::<To>(transaction)
}

fn validate_physical_indexes(
    transaction: &Transaction<'_>,
    schema: &CollectionSchema,
) -> Result<()> {
    for index in schema.indexes() {
        validate_physical_index(transaction, schema.table(), index)?;
    }
    Ok(())
}

fn validate_physical_index(
    transaction: &Transaction<'_>,
    table: &str,
    expected: &IndexSchema,
) -> Result<()> {
    let mut statement = transaction.prepare(PHYSICAL_INDEX_SQL)?;
    let mut rows = statement.query(&[ValueRef::Text(table), ValueRef::Text(expected.name())])?;
    let Some(row) = rows.next_row()? else {
        return Err(schema_mismatch());
    };
    let unique = row.get_i64(0)? != 0;
    let origin = row.get_text(1)?;
    let partial = row.get_i64(2)? != 0;
    if unique != expected.is_unique() || origin != "c" || partial || rows.next_row()?.is_some() {
        return Err(schema_mismatch());
    }
    drop(rows);
    drop(statement);

    let mut statement = transaction.prepare(PHYSICAL_INDEX_COLUMNS_SQL)?;
    let mut rows = statement.query(&[ValueRef::Text(expected.name())])?;
    let mut columns = Vec::with_capacity(expected.columns().len());
    while let Some(row) = rows.next_row()? {
        if row.get_i64(3)? == 0 {
            continue;
        }
        let name = row.get_text(0)?;
        let descending = row.get_i64(1)? != 0;
        let collation = row.get_text(2)?;
        if name.is_empty() || descending || !collation.eq_ignore_ascii_case("BINARY") {
            return Err(schema_mismatch());
        }
        columns.push(name.to_owned());
    }
    if columns
        .iter()
        .map(String::as_str)
        .eq(expected.columns().iter().copied())
    {
        Ok(())
    } else {
        Err(schema_mismatch())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RegisteredMigration {
    from_version: u32,
    to_version: u32,
    from_fingerprint: SchemaFingerprint,
    to_fingerprint: SchemaFingerprint,
}

fn find_migration(
    transaction: &Transaction<'_>,
    collection_id: &str,
    migration_id: &str,
) -> Result<Option<RegisteredMigration>> {
    let mut statement = transaction.prepare(FIND_MIGRATION_SQL)?;
    let mut rows =
        statement.query(&[ValueRef::Text(collection_id), ValueRef::Text(migration_id)])?;
    let Some(row) = rows.next_row()? else {
        return Ok(None);
    };
    let migration = RegisteredMigration {
        from_version: u32::try_from(row.get_i64(0)?).map_err(|_| migration_mismatch())?,
        to_version: u32::try_from(row.get_i64(1)?).map_err(|_| migration_mismatch())?,
        from_fingerprint: fingerprint_from_slice(row.get_blob(2)?)?,
        to_fingerprint: fingerprint_from_slice(row.get_blob(3)?)?,
    };
    if rows.next_row()?.is_some() {
        return Err(migration_mismatch());
    }
    Ok(Some(migration))
}

fn insert_migration(
    transaction: &Transaction<'_>,
    migration_id: &str,
    from: &CollectionSchema,
    to: &CollectionSchema,
) -> Result<()> {
    transaction.execute(
        INSERT_MIGRATION_SQL,
        &[
            ValueRef::Text(from.id()),
            ValueRef::Text(migration_id),
            ValueRef::Integer(i64::from(from.version())),
            ValueRef::Integer(i64::from(to.version())),
            ValueRef::Blob(from.fingerprint().as_bytes()),
            ValueRef::Blob(to.fingerprint().as_bytes()),
        ],
    )?;
    Ok(())
}

fn ensure_migration_matches(
    actual: &RegisteredMigration,
    from: &CollectionSchema,
    to: &CollectionSchema,
) -> Result<()> {
    if actual.from_version == from.version()
        && actual.to_version == to.version()
        && actual.from_fingerprint == from.fingerprint()
        && actual.to_fingerprint == to.fingerprint()
    {
        Ok(())
    } else {
        Err(migration_mismatch())
    }
}

fn ensure_table_is_unclaimed(
    transaction: &Transaction<'_>,
    schema: &CollectionSchema,
) -> Result<()> {
    let mut statement = transaction.prepare(FIND_TABLE_OWNER_SQL)?;
    let mut rows =
        statement.query(&[ValueRef::Text(schema.table()), ValueRef::Text(schema.id())])?;
    let Some(row) = rows.next_row()? else {
        return Err(schema_mismatch());
    };
    if row.get_i64(0)? == 0 {
        Ok(())
    } else {
        Err(schema_mismatch())
    }
}

fn ensure_registered_schema_matches(
    expected: &CollectionSchema,
    registered: &RegisteredSchema,
) -> Result<()> {
    if registered.collection_id() == expected.id()
        && registered.table_name() == expected.table()
        && registered.version() == expected.version()
        && registered.fingerprint() == expected.fingerprint()
        && registered.canonical_descriptor() == expected.canonical_descriptor()
    {
        Ok(())
    } else {
        Err(schema_mismatch())
    }
}

#[derive(Debug, Eq, PartialEq)]
struct PhysicalColumn {
    name: String,
    declared_type: String,
    not_null: bool,
    primary_key: bool,
}

fn validate_physical_table(transaction: &Transaction<'_>, schema: &CollectionSchema) -> Result<()> {
    let mut statement = transaction.prepare(TABLE_COLUMNS_SQL)?;
    let mut rows = statement.query(&[ValueRef::Text(schema.table())])?;
    let mut actual = Vec::with_capacity(schema.fields().len());
    while let Some(row) = rows.next_row()? {
        actual.push(PhysicalColumn {
            name: row.get_text(0)?.to_owned(),
            declared_type: row.get_text(1)?.to_owned(),
            not_null: row.get_i64(2)? != 0,
            primary_key: row.get_i64(3)? != 0,
        });
    }
    if actual.len() != schema.fields().len() {
        return Err(schema_mismatch());
    }

    for expected in schema.fields() {
        let Some(actual) = actual
            .iter()
            .find(|column| column.name == expected.column())
        else {
            return Err(schema_mismatch());
        };
        let expected_not_null = !expected.is_nullable();
        if !actual
            .declared_type
            .eq_ignore_ascii_case(expected.column_type().sql_name())
            || actual.not_null != expected_not_null
            || actual.primary_key != expected.is_primary_key()
        {
            return Err(schema_mismatch());
        }
    }
    Ok(())
}

pub(crate) fn validate_collection_schema(schema: &CollectionSchema) -> Result<()> {
    if !valid_name(schema.id())
        || !valid_name(schema.table())
        || schema.table().starts_with("__csgdb_")
        || schema.version() == 0
        || schema.fields().is_empty()
        || schema.canonical_descriptor().is_empty()
    {
        return Err(invalid_schema());
    }

    let mut ids = HashSet::with_capacity(schema.fields().len());
    let mut columns = HashSet::with_capacity(schema.fields().len());
    let mut primary_keys = 0_u8;
    for field in schema.fields() {
        if !valid_name(field.id())
            || !valid_name(field.column())
            || !ids.insert(field.id())
            || !columns.insert(field.column())
        {
            return Err(invalid_schema());
        }
        if field.is_primary_key() {
            primary_keys = primary_keys.saturating_add(1);
            if field.is_nullable() {
                return Err(invalid_schema());
            }
        }
    }
    if primary_keys != 1 {
        return Err(invalid_schema());
    }

    let mut index_ids = HashSet::with_capacity(schema.indexes().len());
    let mut index_names = HashSet::with_capacity(schema.indexes().len());
    for index in schema.indexes() {
        if !valid_name(index.id())
            || !valid_name(index.name())
            || index.name().starts_with("__csgdb_")
            || index.columns().is_empty()
            || index.canonical_descriptor().is_empty()
            || !index_ids.insert(index.id())
            || !index_names.insert(index.name())
        {
            return Err(invalid_schema());
        }
        let mut index_columns = HashSet::with_capacity(index.columns().len());
        for column in index.columns() {
            if !columns.contains(column) || !index_columns.insert(*column) {
                return Err(invalid_schema());
            }
        }
    }
    Ok(())
}

fn validate_migration(
    from: &CollectionSchema,
    to: &CollectionSchema,
    migration_id: &str,
) -> Result<()> {
    validate_collection_schema(from)?;
    validate_collection_schema(to)?;
    if !valid_name(migration_id)
        || from.id() != to.id()
        || to.version() <= from.version()
        || from.fingerprint() == to.fingerprint()
    {
        Err(invalid_migration())
    } else {
        Ok(())
    }
}

fn valid_name(value: &str) -> bool {
    !value.is_empty() && value.len() <= 255 && !value.contains('\0')
}

fn quote_sql_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn schema_registry_exists(database: &Database) -> Result<bool> {
    Ok(database.query_i64(
        "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name = '__csgdb_schema'",
    )? == 1)
}

fn schema_registry_exists_on_reader(connection: &ReadConnection<'_>) -> Result<bool> {
    Ok(connection.query_i64(
        "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name = '__csgdb_schema'",
    )? == 1)
}

fn fingerprint_from_slice(bytes: &[u8]) -> Result<SchemaFingerprint> {
    let bytes = <[u8; 32]>::try_from(bytes).map_err(|_| schema_mismatch())?;
    Ok(SchemaFingerprint::new(bytes))
}

fn invalid_field_type() -> Error {
    Error::new(
        ErrorCode::InvalidColumnType,
        "collection field has an incompatible database storage class",
    )
}

fn invalid_field_value() -> Error {
    Error::new(
        ErrorCode::InvalidFieldValue,
        "collection field value cannot be represented by its database type",
    )
}

fn invalid_schema() -> Error {
    Error::new(
        ErrorCode::InvalidSchema,
        "collection schema metadata is invalid",
    )
}

fn schema_mismatch() -> Error {
    Error::new(
        ErrorCode::SchemaMismatch,
        "collection schema does not match the registered database schema",
    )
}

fn invalid_migration() -> Error {
    Error::new(
        ErrorCode::InvalidMigration,
        "collection migration endpoints or identifier are invalid",
    )
}

fn migration_mismatch() -> Error {
    Error::new(
        ErrorCode::MigrationMismatch,
        "collection migration does not match the registered database state",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PoolOptions, SecretString};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DATABASE: AtomicU64 = AtomicU64::new(1);

    struct TestDatabasePath {
        path: PathBuf,
    }

    impl TestDatabasePath {
        fn new(name: &str) -> Self {
            let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "csgdb-collection-{name}-{}-{sequence}.db",
                std::process::id()
            ));
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestDatabasePath {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
            let base = self.path.to_string_lossy();
            let _ = fs::remove_file(format!("{base}-wal"));
            let _ = fs::remove_file(format!("{base}-shm"));
        }
    }

    #[derive(Clone, Debug, PartialEq, crate::Collection)]
    #[csgdb(collection = "agent.memory", table = "agent_memory", version = 1)]
    struct Memory {
        #[csgdb(id = "agent.memory.id", column = "id", primary_key)]
        id: i64,
        #[csgdb(id = "agent.memory.text", column = "text")]
        text: String,
        #[csgdb(id = "agent.memory.score", column = "score")]
        score: Option<f64>,
        #[csgdb(id = "agent.memory.active", column = "active")]
        active: bool,
        #[csgdb(id = "agent.memory.payload", column = "payload")]
        payload: Vec<u8>,
    }

    #[derive(Clone, Debug, PartialEq, crate::Collection)]
    #[csgdb(collection = "agent.memory", table = "agent_memory", version = 1)]
    struct RenamedMemory {
        #[csgdb(id = "agent.memory.payload", column = "payload")]
        bytes: Vec<u8>,
        #[csgdb(id = "agent.memory.active", column = "active")]
        enabled: bool,
        #[csgdb(id = "agent.memory.id", column = "id", primary_key)]
        identifier: i64,
        #[csgdb(id = "agent.memory.score", column = "score")]
        importance: Option<f64>,
        #[csgdb(id = "agent.memory.text", column = "text")]
        body: String,
    }

    #[derive(Clone, Debug, PartialEq, crate::Collection)]
    #[csgdb(collection = "agent.memory", table = "agent_memory", version = 2)]
    struct IncompatibleMemory {
        #[csgdb(id = "agent.memory.id", column = "id", primary_key)]
        id: i64,
        #[csgdb(id = "agent.memory.text", column = "text")]
        text: String,
    }

    #[derive(Clone, Debug, PartialEq, crate::Collection)]
    #[csgdb(collection = "agent.memory.alias", table = "agent_memory", version = 1)]
    struct AliasedMemory {
        #[csgdb(id = "agent.memory.alias.id", column = "id", primary_key)]
        id: i64,
        #[csgdb(id = "agent.memory.alias.text", column = "text")]
        text: String,
        #[csgdb(id = "agent.memory.alias.score", column = "score")]
        score: Option<f64>,
        #[csgdb(id = "agent.memory.alias.active", column = "active")]
        active: bool,
        #[csgdb(id = "agent.memory.alias.payload", column = "payload")]
        payload: Vec<u8>,
    }

    #[derive(Clone, Debug, PartialEq, crate::Collection)]
    #[csgdb(
        collection = "agent.counter",
        table = "agent_counter",
        version = 1,
        crate = "crate"
    )]
    struct Counter {
        #[csgdb(id = "agent.counter.id", column = "id", primary_key)]
        id: i64,
        #[csgdb(id = "agent.counter.value", column = "value")]
        value: u64,
    }

    #[derive(Clone, Debug, PartialEq, crate::Collection)]
    #[csgdb(
        collection = "agent.indexed-memory",
        table = "indexed_memory",
        version = 1,
        index(
            id = "agent.indexed-memory.namespace-external",
            name = "idx_indexed_memory_namespace_external",
            field = "agent.indexed-memory.namespace",
            field = "agent.indexed-memory.external",
            unique
        ),
        index(
            id = "agent.indexed-memory.score",
            name = "idx_indexed_memory_score",
            field = "agent.indexed-memory.score"
        )
    )]
    struct IndexedMemory {
        #[csgdb(id = "agent.indexed-memory.id", column = "id", primary_key)]
        id: i64,
        #[csgdb(id = "agent.indexed-memory.namespace", column = "namespace")]
        namespace: String,
        #[csgdb(id = "agent.indexed-memory.external", column = "external_key")]
        external_key: String,
        #[csgdb(id = "agent.indexed-memory.score", column = "score")]
        score: f64,
    }

    #[derive(Clone, Debug, PartialEq, crate::Collection)]
    #[csgdb(
        collection = "agent.indexed-memory",
        table = "indexed_memory",
        version = 1
    )]
    struct IndexedMemoryWithoutIndexes {
        #[csgdb(id = "agent.indexed-memory.id", column = "id", primary_key)]
        id: i64,
        #[csgdb(id = "agent.indexed-memory.namespace", column = "namespace")]
        namespace: String,
        #[csgdb(id = "agent.indexed-memory.external", column = "external_key")]
        external_key: String,
        #[csgdb(id = "agent.indexed-memory.score", column = "score")]
        score: f64,
    }

    #[derive(Clone, Debug, PartialEq, crate::Collection)]
    #[csgdb(
        collection = "agent.migrating-memory",
        table = "migrating_memory",
        version = 1
    )]
    struct MigratingMemoryV1 {
        #[csgdb(id = "agent.migrating-memory.id", column = "id", primary_key)]
        id: i64,
        #[csgdb(id = "agent.migrating-memory.text", column = "text")]
        text: String,
    }

    #[derive(Clone, Debug, PartialEq, crate::Collection)]
    #[csgdb(
        collection = "agent.migrating-memory",
        table = "migrating_memory",
        version = 2,
        index(
            id = "agent.migrating-memory.score",
            name = "idx_migrating_memory_score",
            field = "agent.migrating-memory.score"
        )
    )]
    struct MigratingMemoryV2 {
        #[csgdb(id = "agent.migrating-memory.id", column = "id", primary_key)]
        id: i64,
        #[csgdb(id = "agent.migrating-memory.text", column = "text")]
        text: String,
        #[csgdb(id = "agent.migrating-memory.score", column = "score")]
        score: Option<f64>,
    }

    #[derive(Clone, Debug, PartialEq, crate::Collection)]
    #[csgdb(
        collection = "agent.migrating-memory",
        table = "migrating_memory",
        version = 3
    )]
    struct MigratingMemoryV3 {
        #[csgdb(id = "agent.migrating-memory.id", column = "id", primary_key)]
        id: i64,
        #[csgdb(id = "agent.migrating-memory.text", column = "text")]
        text: String,
        #[csgdb(id = "agent.migrating-memory.score", column = "score")]
        score: Option<f64>,
        #[csgdb(id = "agent.migrating-memory.tag", column = "tag")]
        tag: Option<String>,
    }

    fn memory(id: i64, text: &str) -> Memory {
        Memory {
            id,
            text: text.to_owned(),
            score: Some(0.75),
            active: true,
            payload: vec![1, 2, 3],
        }
    }

    #[test]
    fn generated_field_constants_preserve_type_and_storage_identity() {
        fn accepts_memory_text(_: CollectionField<Memory, String>) {}

        accepts_memory_text(Memory::FIELD_TEXT);
        assert_eq!(
            std::mem::size_of_val(&Memory::FIELD_TEXT),
            std::mem::size_of::<&FieldSchema>()
        );
        assert_eq!(Memory::FIELD_ID.id(), "agent.memory.id");
        assert_eq!(Memory::FIELD_ID.column(), "id");
        assert_eq!(Memory::FIELD_ID.column_type(), ColumnType::Integer);
        assert!(Memory::FIELD_ID.is_primary_key());
        assert!(!Memory::FIELD_ID.is_nullable());
        assert_eq!(Memory::FIELD_TEXT.collection(), Memory::schema());
        assert!(Memory::FIELD_TEXT.same_storage_field(&RenamedMemory::FIELD_BODY));
        assert!(Memory::FIELD_PAYLOAD.same_storage_field(&RenamedMemory::FIELD_BYTES));
        assert!(!Memory::FIELD_TEXT.same_storage_field(&Memory::FIELD_SCORE));
        assert_eq!(
            Memory::FIELD_TEXT.encode(&"typed field".to_owned()),
            Ok(Value::Text("typed field".to_owned()))
        );
        assert_eq!(Memory::FIELD_SCORE.encode(&None), Ok(Value::Null));
    }

    #[test]
    fn stable_metadata_survives_rust_renames_and_field_reordering() {
        assert_eq!(
            Memory::schema().fingerprint(),
            RenamedMemory::schema().fingerprint()
        );
        assert_eq!(
            Memory::schema().canonical_descriptor(),
            RenamedMemory::schema().canonical_descriptor()
        );

        let path = TestDatabasePath::new("stable");
        let mut database =
            Database::open_with_passphrase(path.path(), "typed-schema-secret").expect("open");
        assert_eq!(
            database.register_collection::<Memory>().expect("register"),
            SchemaRegistration::Created
        );
        assert_eq!(
            database
                .register_collection::<RenamedMemory>()
                .expect("validate renamed type"),
            SchemaRegistration::AlreadyRegistered
        );

        let registered = database
            .registered_schema("agent.memory")
            .expect("query registry")
            .expect("registered metadata");
        assert_eq!(registered.table_name(), "agent_memory");
        assert_eq!(registered.version(), 1);
        assert_eq!(registered.fingerprint(), Memory::schema().fingerprint());

        let original = memory(7, "typed-memory-plaintext-marker");
        assert_eq!(database.insert(&original).expect("insert"), 1);
        let renamed = database
            .get::<RenamedMemory>(&7)
            .expect("get renamed")
            .expect("record");
        assert_eq!(renamed.identifier, 7);
        assert_eq!(renamed.body, original.text);
        assert_eq!(renamed.bytes, original.payload);

        let updated = RenamedMemory {
            body: "updated".to_owned(),
            importance: None,
            ..renamed
        };
        assert_eq!(database.update(&updated).expect("update"), 1);
        let round_trip = database
            .get::<Memory>(&7)
            .expect("get original")
            .expect("record");
        assert_eq!(round_trip.text, "updated");
        assert_eq!(round_trip.score, None);
        assert_eq!(database.delete::<Memory>(&7).expect("delete"), 1);
        assert_eq!(database.get::<Memory>(&7).expect("missing"), None);
        database.close().expect("close");

        let encrypted = fs::read(path.path()).expect("read encrypted database");
        assert!(!encrypted
            .windows(b"typed-memory-plaintext-marker".len())
            .any(|window| window == b"typed-memory-plaintext-marker"));
    }

    #[test]
    fn typed_crud_works_in_transactions_and_the_managed_pool() {
        let path = TestDatabasePath::new("pool");
        let mut database =
            Database::open_with_passphrase(path.path(), "typed-pool-secret").expect("open");
        database.register_collection::<Memory>().expect("register");
        {
            let transaction = database.transaction().expect("transaction");
            CollectionCrud::insert(&transaction, &memory(1, "rollback")).expect("insert");
            transaction.rollback().expect("rollback");
        }
        assert_eq!(database.get::<Memory>(&1).expect("get"), None);
        {
            let transaction = database.transaction().expect("transaction");
            CollectionCrud::insert(&transaction, &memory(2, "committed")).expect("insert");
            assert_eq!(
                CollectionCrud::get::<Memory>(&transaction, &2)
                    .expect("get")
                    .expect("record")
                    .text,
                "committed"
            );
            transaction.commit().expect("commit");
        }
        database.close().expect("close");

        let pool = DatabasePool::open_with_passphrase(
            path.path(),
            "typed-pool-secret",
            PoolOptions::default(),
        )
        .expect("open pool");
        assert_eq!(
            pool.register_collection::<Memory>().expect("validate"),
            SchemaRegistration::AlreadyRegistered
        );
        assert_eq!(pool.insert(&memory(3, "pooled")).expect("insert"), 1);
        let mut pooled = pool.get::<Memory>(&3).expect("get").expect("record");
        pooled.text = "pooled-update".to_owned();
        assert_eq!(pool.update(&pooled).expect("update"), 1);
        pool.read(|connection| {
            let snapshot = connection.transaction()?;
            assert_eq!(
                snapshot.get::<Memory>(&3)?.expect("record").text,
                "pooled-update"
            );
            snapshot.commit()
        })
        .expect("snapshot read");
        assert_eq!(pool.delete::<Memory>(&3).expect("delete"), 1);
        assert_eq!(
            pool.registered_schema("agent.memory")
                .expect("registered")
                .expect("metadata")
                .fingerprint(),
            Memory::schema().fingerprint()
        );
        pool.close().expect("close pool");
    }

    #[test]
    fn registration_rejects_fingerprint_and_physical_schema_mismatches() {
        let path = TestDatabasePath::new("mismatch");
        let mut database =
            Database::open_with_passphrase(path.path(), "typed-mismatch-secret").expect("open");
        database
            .execute_batch(
                "CREATE TABLE agent_memory (id INTEGER NOT NULL PRIMARY KEY, text BLOB NOT NULL)",
            )
            .expect("create incompatible table");
        let error = database
            .register_collection::<Memory>()
            .expect_err("incompatible table must fail");
        assert_eq!(error.code(), ErrorCode::SchemaMismatch);
        assert_eq!(
            database
                .registered_schema("agent.memory")
                .expect("registry state"),
            None
        );

        database
            .execute_batch("DROP TABLE agent_memory")
            .expect("drop incompatible table");
        database
            .register_collection::<Memory>()
            .expect("register compatible table");
        let original = database
            .registered_schema("agent.memory")
            .expect("registry")
            .expect("metadata");
        let error = database
            .register_collection::<IncompatibleMemory>()
            .expect_err("fingerprint mismatch must fail");
        assert_eq!(error.code(), ErrorCode::SchemaMismatch);
        assert_eq!(
            database
                .registered_schema("agent.memory")
                .expect("registry")
                .expect("metadata"),
            original
        );
        let error = database
            .register_collection::<AliasedMemory>()
            .expect_err("a physical table must have one stable owner");
        assert_eq!(error.code(), ErrorCode::SchemaMismatch);
        assert_eq!(
            database
                .registered_schema("agent.memory.alias")
                .expect("registry"),
            None
        );
    }

    #[test]
    fn field_decoding_rejects_out_of_domain_values() {
        assert_eq!(
            <u64 as FieldValue>::to_value(&u64::MAX)
                .expect_err("u64 overflow")
                .code(),
            ErrorCode::InvalidFieldValue
        );

        let path = TestDatabasePath::new("invalid-field");
        let mut database =
            Database::open_with_passphrase(path.path(), "typed-field-secret").expect("open");
        database
            .register_collection::<Memory>()
            .expect("register memory");
        database.insert(&memory(9, "invalid-bool")).expect("insert");
        database
            .execute("UPDATE agent_memory SET active = 2 WHERE id = 9", &[])
            .expect("corrupt logical bool");
        assert_eq!(
            database
                .get::<Memory>(&9)
                .expect_err("invalid bool must fail")
                .code(),
            ErrorCode::InvalidFieldValue
        );

        database
            .register_collection::<Counter>()
            .expect("register counter");
        assert_eq!(
            database
                .insert(&Counter {
                    id: 1,
                    value: u64::MAX,
                })
                .expect_err("overflow must fail")
                .code(),
            ErrorCode::InvalidFieldValue
        );
    }

    #[test]
    fn stable_indexes_are_registered_enforced_and_physically_validated() {
        assert_eq!(
            IndexedMemory::schema().fingerprint(),
            IndexedMemoryWithoutIndexes::schema().fingerprint()
        );
        let indexes = IndexedMemory::schema().indexes();
        assert_eq!(indexes.len(), 2);
        assert_eq!(indexes[0].columns(), &["namespace", "external_key"]);
        assert!(indexes[0].is_unique());
        assert_eq!(indexes[1].columns(), &["score"]);
        assert!(!indexes[1].is_unique());

        let path = TestDatabasePath::new("indexes");
        let mut database =
            Database::open_with_passphrase(path.path(), "typed-index-secret").expect("open");
        assert_eq!(
            database
                .register_collection::<IndexedMemory>()
                .expect("register"),
            SchemaRegistration::Created
        );
        database
            .insert(&IndexedMemory {
                id: 1,
                namespace: "session".to_owned(),
                external_key: "memory-1".to_owned(),
                score: 0.8,
            })
            .expect("insert first");
        let duplicate = database
            .insert(&IndexedMemory {
                id: 2,
                namespace: "session".to_owned(),
                external_key: "memory-1".to_owned(),
                score: 0.2,
            })
            .expect_err("composite unique index must be enforced");
        assert_eq!(duplicate.code(), ErrorCode::ConstraintViolation);
        assert_eq!(
            database
                .register_collection::<IndexedMemoryWithoutIndexes>()
                .expect_err("index declarations are an exact registered contract")
                .code(),
            ErrorCode::SchemaMismatch
        );
        database.close().expect("close");

        let mut database =
            Database::open_with_passphrase(path.path(), "typed-index-secret").expect("reopen");
        assert_eq!(
            database
                .register_collection::<IndexedMemory>()
                .expect("validate indexes after reopen"),
            SchemaRegistration::AlreadyRegistered
        );
        database
            .execute_batch(
                "DROP INDEX idx_indexed_memory_namespace_external;
                 CREATE UNIQUE INDEX idx_indexed_memory_namespace_external
                 ON indexed_memory (external_key, namespace)",
            )
            .expect("replace with wrong column order");
        assert_eq!(
            database
                .register_collection::<IndexedMemory>()
                .expect_err("physical index drift must fail")
                .code(),
            ErrorCode::SchemaMismatch
        );
    }

    #[test]
    fn explicit_migration_is_atomic_idempotent_and_survives_reopen() {
        let path = TestDatabasePath::new("migration-success");
        let mut database =
            Database::open_with_passphrase(path.path(), "typed-migration-secret").expect("open");
        database
            .register_collection::<MigratingMemoryV1>()
            .expect("register v1");
        database
            .insert(&MigratingMemoryV1 {
                id: 1,
                text: "migration-plaintext-marker".to_owned(),
            })
            .expect("insert v1");

        assert_eq!(
            database
                .migrate_collection::<MigratingMemoryV1, MigratingMemoryV2, _>(
                    "add-score-v2",
                    |transaction| {
                        transaction.execute_batch(
                            "ALTER TABLE migrating_memory ADD COLUMN score REAL;
                             UPDATE migrating_memory SET score = 0.9 WHERE id = 1",
                        )
                    },
                )
                .expect("migrate"),
            MigrationStatus::Applied
        );
        assert_eq!(
            database
                .get::<MigratingMemoryV2>(&1)
                .expect("get v2")
                .expect("migrated row")
                .score,
            Some(0.9)
        );
        let mut callback_called = false;
        assert_eq!(
            database
                .migrate_collection::<MigratingMemoryV1, MigratingMemoryV2, _>(
                    "add-score-v2",
                    |_| {
                        callback_called = true;
                        Ok(())
                    },
                )
                .expect("repeat migration"),
            MigrationStatus::AlreadyApplied
        );
        assert!(!callback_called);
        assert_eq!(
            database
                .register_collection::<MigratingMemoryV1>()
                .expect_err("old schema must no longer register")
                .code(),
            ErrorCode::SchemaMismatch
        );
        database.close().expect("close");

        let mut database =
            Database::open_with_passphrase(path.path(), "typed-migration-secret").expect("reopen");
        assert_eq!(
            database
                .register_collection::<MigratingMemoryV2>()
                .expect("validate v2"),
            SchemaRegistration::AlreadyRegistered
        );
        assert_eq!(
            database
                .migrate_collection::<MigratingMemoryV1, MigratingMemoryV2, _>(
                    "add-score-v2",
                    |_| panic!("idempotent migration callback must not run"),
                )
                .expect("repeat after reopen"),
            MigrationStatus::AlreadyApplied
        );
        assert_eq!(
            database
                .migrate_collection::<MigratingMemoryV2, MigratingMemoryV3, _>(
                    "add-score-v2",
                    |_| Ok(()),
                )
                .expect_err("a migration ID cannot be reused")
                .code(),
            ErrorCode::MigrationMismatch
        );
        database.close().expect("close again");

        let encrypted = fs::read(path.path()).expect("read encrypted database");
        assert!(!encrypted
            .windows(b"migration-plaintext-marker".len())
            .any(|window| window == b"migration-plaintext-marker"));
    }

    #[test]
    fn failed_and_panicking_migrations_roll_back_schema_data_and_history() {
        let path = TestDatabasePath::new("migration-rollback");
        let mut database =
            Database::open_with_passphrase(path.path(), "migration-rollback-secret").expect("open");
        database
            .register_collection::<MigratingMemoryV1>()
            .expect("register v1");
        database
            .insert(&MigratingMemoryV1 {
                id: 1,
                text: "original".to_owned(),
            })
            .expect("insert");

        let error = database
            .migrate_collection::<MigratingMemoryV1, MigratingMemoryV2, _>(
                "failing-add-score-v2",
                |transaction| {
                    transaction.execute_batch(
                        "ALTER TABLE migrating_memory ADD COLUMN score REAL;
                         UPDATE migrating_memory SET text = 'changed' WHERE id = 1",
                    )?;
                    Err(Error::new(ErrorCode::Storage, "injected migration failure"))
                },
            )
            .expect_err("callback error must roll back");
        assert_eq!(error.code(), ErrorCode::Storage);
        assert_eq!(
            database
                .get::<MigratingMemoryV1>(&1)
                .expect("read after rollback")
                .expect("row")
                .text,
            "original"
        );
        assert_eq!(
            database
                .register_collection::<MigratingMemoryV1>()
                .expect("v1 remains current"),
            SchemaRegistration::AlreadyRegistered
        );

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = database.migrate_collection::<MigratingMemoryV1, MigratingMemoryV2, _>(
                "panicking-add-score-v2",
                |transaction| {
                    transaction
                        .execute_batch("ALTER TABLE migrating_memory ADD COLUMN score REAL")?;
                    panic!("injected migration panic")
                },
            );
        }));
        assert!(panic.is_err());
        assert_eq!(
            database
                .register_collection::<MigratingMemoryV1>()
                .expect("panic rollback keeps v1"),
            SchemaRegistration::AlreadyRegistered
        );
        assert_eq!(
            database
                .query_i64("SELECT count(*) FROM __csgdb_migration")
                .expect("migration history"),
            0
        );
        database.close().expect("close");

        let mut database = Database::open_with_passphrase(path.path(), "migration-rollback-secret")
            .expect("reopen");
        assert_eq!(
            database
                .register_collection::<MigratingMemoryV1>()
                .expect("v1 validates after reopen"),
            SchemaRegistration::AlreadyRegistered
        );
        assert_eq!(
            database
                .migrate_collection::<MigratingMemoryV2, MigratingMemoryV1, _>("backwards", |_| Ok(
                    ()
                ),)
                .expect_err("versions must increase")
                .code(),
            ErrorCode::InvalidMigration
        );
    }

    #[test]
    fn registration_upgrades_a_legacy_schema_registry_without_changing_data() {
        let path = TestDatabasePath::new("legacy-registry");
        let mut database =
            Database::open_with_passphrase(path.path(), "legacy-registry-secret").expect("open");
        database
            .execute_batch(MigratingMemoryV1::CREATE_TABLE_SQL)
            .expect("create legacy table");
        database
            .execute_batch(SCHEMA_REGISTRY_SQL)
            .expect("create legacy registry");
        let schema = MigratingMemoryV1::schema();
        database
            .execute(
                INSERT_REGISTERED_SCHEMA_SQL,
                &[
                    ValueRef::Text(schema.id()),
                    ValueRef::Text(schema.table()),
                    ValueRef::Integer(i64::from(schema.version())),
                    ValueRef::Blob(schema.fingerprint().as_bytes()),
                    ValueRef::Text(schema.canonical_descriptor()),
                ],
            )
            .expect("insert legacy metadata");
        database
            .insert(&MigratingMemoryV1 {
                id: 1,
                text: "legacy-row".to_owned(),
            })
            .expect("insert legacy row");

        assert_eq!(
            database
                .register_collection::<MigratingMemoryV1>()
                .expect("adopt legacy registry"),
            SchemaRegistration::AlreadyRegistered
        );
        assert_eq!(
            database
                .get::<MigratingMemoryV1>(&1)
                .expect("read legacy row")
                .expect("row")
                .text,
            "legacy-row"
        );
        assert_eq!(
            database
                .query_i64(
                    "SELECT count(*) FROM sqlite_schema
                     WHERE type = 'table'
                     AND name IN ('__csgdb_index_schema', '__csgdb_migration')",
                )
                .expect("auxiliary registries"),
            2
        );
    }

    #[test]
    fn managed_pool_runs_migrations_as_exclusive_writes() {
        let path = TestDatabasePath::new("pool-migration");
        let mut database =
            Database::open_with_passphrase(path.path(), "pool-migration-secret").expect("open");
        database
            .register_collection::<MigratingMemoryV1>()
            .expect("register v1");
        database.close().expect("close database");

        let pool = DatabasePool::open_with_passphrase(
            path.path(),
            "pool-migration-secret",
            PoolOptions::default(),
        )
        .expect("open pool");
        assert_eq!(
            pool.migrate_collection::<MigratingMemoryV1, MigratingMemoryV2, _>(
                "pool-add-score-v2",
                |transaction| transaction
                    .execute_batch("ALTER TABLE migrating_memory ADD COLUMN score REAL"),
            )
            .expect("pool migration"),
            MigrationStatus::Applied
        );
        pool.insert(&MigratingMemoryV2 {
            id: 7,
            text: "pooled migration".to_owned(),
            score: Some(0.7),
        })
        .expect("insert v2");
        assert_eq!(
            pool.get::<MigratingMemoryV2>(&7)
                .expect("get v2")
                .expect("row")
                .score,
            Some(0.7)
        );
        pool.close().expect("close pool");
    }

    #[test]
    fn missing_schema_registry_is_reported_as_unregistered() {
        let path = TestDatabasePath::new("unregistered");
        let database = Database::open_with_key(
            path.path(),
            crate::KeySource::Passphrase(SecretString::new("registry-secret")),
        )
        .expect("open");
        assert_eq!(
            database.registered_schema("agent.missing").expect("query"),
            None
        );
    }
}
