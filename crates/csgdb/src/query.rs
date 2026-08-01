use crate::collection::validate_collection_schema;
use crate::{
    Collection, CollectionField, ColumnType, Database, DatabasePool, Error, ErrorCode, FieldSchema,
    FieldValue, ReadConnection, ReadTransaction, Result, Statement, Transaction, Value,
};
use std::collections::HashSet;
use std::marker::PhantomData;

/// Largest result set accepted by the typed collection query API.
pub const MAX_QUERY_LIMIT: u32 = 10_000;
/// Largest predicate tree accepted by one typed collection query.
pub const MAX_PREDICATE_NODES: usize = 256;
/// Largest nesting depth accepted by one typed collection query.
pub const MAX_PREDICATE_DEPTH: usize = 32;
/// Largest number of caller-specified ordering fields accepted by one query.
pub const MAX_QUERY_ORDER_FIELDS: usize = 8;

/// Conversion into an owned, parameter-bound query value.
///
/// This trait lets text and blobs be supplied either as owned values or as
/// borrowed `&str`/`&[u8]` values without weakening the field's Rust type.
pub trait IntoQueryValue<T: FieldValue> {
    /// Converts this input into the database representation for `T`.
    ///
    /// # Errors
    ///
    /// Returns an error when the value cannot be represented by `T`.
    fn into_query_value(self) -> Result<Value>;
}

impl<T: FieldValue> IntoQueryValue<T> for T {
    fn into_query_value(self) -> Result<Value> {
        self.to_value()
    }
}

impl IntoQueryValue<String> for &str {
    fn into_query_value(self) -> Result<Value> {
        Ok(Value::Text(self.to_owned()))
    }
}

impl IntoQueryValue<String> for &String {
    fn into_query_value(self) -> Result<Value> {
        Ok(Value::Text(self.clone()))
    }
}

impl IntoQueryValue<Vec<u8>> for &[u8] {
    fn into_query_value(self) -> Result<Value> {
        Ok(Value::Blob(self.to_owned()))
    }
}

impl IntoQueryValue<Vec<u8>> for &Vec<u8> {
    fn into_query_value(self) -> Result<Value> {
        Ok(Value::Blob(self.clone()))
    }
}

/// Marker for field types with a stable total database ordering.
pub trait OrderedFieldValue: FieldValue {}

macro_rules! ordered_field_values {
    ($($type:ty),+ $(,)?) => {
        $(impl OrderedFieldValue for $type {})+
    };
}

ordered_field_values!(i8, i16, i32, i64, u8, u16, u32, u64, f32, f64, String);

impl<T: OrderedFieldValue> OrderedFieldValue for Option<T> {}

#[derive(Clone, Copy)]
enum Comparison {
    Equal,
    NotEqual,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
}

#[derive(Clone, Copy)]
enum NullComparison {
    IsNull,
    IsNotNull,
}

#[derive(Clone)]
enum PredicateNode {
    Compare {
        field: &'static FieldSchema,
        comparison: Comparison,
        value: Value,
    },
    Null {
        field: &'static FieldSchema,
        comparison: NullComparison,
    },
    And(Box<Self>, Box<Self>),
    Or(Box<Self>, Box<Self>),
    Not(Box<Self>),
}

/// An owned, type-checked predicate for collection `C`.
///
/// Predicates deliberately do not implement `Debug`: bound values can contain
/// sensitive Agent data and must not be copied into routine diagnostics.
pub struct Predicate<C> {
    node: PredicateNode,
    marker: PhantomData<fn() -> C>,
}

impl<C> Predicate<C> {
    fn compare(field: &'static FieldSchema, comparison: Comparison, value: Value) -> Self {
        Self {
            node: PredicateNode::Compare {
                field,
                comparison,
                value,
            },
            marker: PhantomData,
        }
    }

    fn null(field: &'static FieldSchema, comparison: NullComparison) -> Self {
        Self {
            node: PredicateNode::Null { field, comparison },
            marker: PhantomData,
        }
    }

    /// Requires both predicates to match.
    #[must_use]
    pub fn and(self, other: Self) -> Self {
        Self {
            node: PredicateNode::And(Box::new(self.node), Box::new(other.node)),
            marker: PhantomData,
        }
    }

    /// Requires either predicate to match.
    #[must_use]
    pub fn or(self, other: Self) -> Self {
        Self {
            node: PredicateNode::Or(Box::new(self.node), Box::new(other.node)),
            marker: PhantomData,
        }
    }

    /// Negates this predicate.
    #[must_use]
    #[allow(clippy::should_implement_trait)]
    pub fn not(self) -> Self {
        Self {
            node: PredicateNode::Not(Box::new(self.node)),
            marker: PhantomData,
        }
    }
}

impl<C> Clone for Predicate<C> {
    fn clone(&self) -> Self {
        Self {
            node: self.node.clone(),
            marker: PhantomData,
        }
    }
}

impl<C: Collection, T: FieldValue> CollectionField<C, T> {
    /// Builds an equality predicate with an owned bound value.
    ///
    /// # Errors
    ///
    /// Returns an error when the input cannot be represented by this field.
    pub fn eq<V: IntoQueryValue<T>>(self, value: V) -> Result<Predicate<C>> {
        Ok(Predicate::compare(
            self.schema(),
            Comparison::Equal,
            value.into_query_value()?,
        ))
    }

    /// Builds an inequality predicate with an owned bound value.
    ///
    /// # Errors
    ///
    /// Returns an error when the input cannot be represented by this field.
    pub fn ne<V: IntoQueryValue<T>>(self, value: V) -> Result<Predicate<C>> {
        Ok(Predicate::compare(
            self.schema(),
            Comparison::NotEqual,
            value.into_query_value()?,
        ))
    }

    /// Orders this field in ascending database order.
    #[must_use]
    pub const fn asc(self) -> QueryOrder<C> {
        QueryOrder::new(self.schema(), OrderDirection::Ascending)
    }

    /// Orders this field in descending database order.
    #[must_use]
    pub const fn desc(self) -> QueryOrder<C> {
        QueryOrder::new(self.schema(), OrderDirection::Descending)
    }
}

impl<C: Collection, T: OrderedFieldValue> CollectionField<C, T> {
    /// Builds a less-than predicate with an owned bound value.
    ///
    /// # Errors
    ///
    /// Returns an error when the input cannot be represented by this field.
    pub fn lt<V: IntoQueryValue<T>>(self, value: V) -> Result<Predicate<C>> {
        Ok(Predicate::compare(
            self.schema(),
            Comparison::Less,
            value.into_query_value()?,
        ))
    }

    /// Builds a less-than-or-equal predicate with an owned bound value.
    ///
    /// # Errors
    ///
    /// Returns an error when the input cannot be represented by this field.
    pub fn le<V: IntoQueryValue<T>>(self, value: V) -> Result<Predicate<C>> {
        Ok(Predicate::compare(
            self.schema(),
            Comparison::LessOrEqual,
            value.into_query_value()?,
        ))
    }

    /// Builds a greater-than predicate with an owned bound value.
    ///
    /// # Errors
    ///
    /// Returns an error when the input cannot be represented by this field.
    pub fn gt<V: IntoQueryValue<T>>(self, value: V) -> Result<Predicate<C>> {
        Ok(Predicate::compare(
            self.schema(),
            Comparison::Greater,
            value.into_query_value()?,
        ))
    }

    /// Builds a greater-than-or-equal predicate with an owned bound value.
    ///
    /// # Errors
    ///
    /// Returns an error when the input cannot be represented by this field.
    pub fn ge<V: IntoQueryValue<T>>(self, value: V) -> Result<Predicate<C>> {
        Ok(Predicate::compare(
            self.schema(),
            Comparison::GreaterOrEqual,
            value.into_query_value()?,
        ))
    }
}

impl<C: Collection, T: FieldValue> CollectionField<C, Option<T>> {
    /// Matches rows where this optional field is null.
    #[must_use]
    pub fn is_null(self) -> Predicate<C> {
        Predicate::null(self.schema(), NullComparison::IsNull)
    }

    /// Matches rows where this optional field is not null.
    #[must_use]
    pub fn is_not_null(self) -> Predicate<C> {
        Predicate::null(self.schema(), NullComparison::IsNotNull)
    }
}

/// Direction for one typed query ordering term.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum OrderDirection {
    Ascending,
    Descending,
}

/// A type-checked ordering term for collection `C`.
pub struct QueryOrder<C> {
    field: &'static FieldSchema,
    direction: OrderDirection,
    marker: PhantomData<fn() -> C>,
}

impl<C> QueryOrder<C> {
    const fn new(field: &'static FieldSchema, direction: OrderDirection) -> Self {
        Self {
            field,
            direction,
            marker: PhantomData,
        }
    }

    #[must_use]
    pub const fn field_id(&self) -> &'static str {
        self.field.id()
    }

    #[must_use]
    pub const fn direction(&self) -> OrderDirection {
        self.direction
    }
}

impl<C> Copy for QueryOrder<C> {}

impl<C> Clone for QueryOrder<C> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<C> std::fmt::Debug for QueryOrder<C> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("QueryOrder")
            .field("field_id", &self.field.id())
            .field("direction", &self.direction)
            .finish()
    }
}

/// A typed query draft that cannot be executed until [`QueryDraft::take`] is called.
pub struct QueryDraft<C> {
    predicate: Option<PredicateNode>,
    order: Vec<QueryOrder<C>>,
    marker: PhantomData<fn() -> C>,
}

impl<C: Collection> QueryDraft<C> {
    #[doc(hidden)]
    #[must_use]
    pub fn new() -> Self {
        Self {
            predicate: None,
            order: Vec::new(),
            marker: PhantomData,
        }
    }

    /// Adds a predicate. Repeated calls are combined with logical `AND`.
    #[must_use]
    pub fn filter(mut self, predicate: Predicate<C>) -> Self {
        self.predicate = Some(match self.predicate {
            Some(existing) => PredicateNode::And(Box::new(existing), Box::new(predicate.node)),
            None => predicate.node,
        });
        self
    }

    /// Appends one typed ordering term.
    #[must_use]
    pub fn order_by(mut self, order: QueryOrder<C>) -> Self {
        self.order.push(order);
        self
    }

    /// Finalizes this query with a mandatory, bounded result limit.
    ///
    /// The resulting plan owns every bound value and can be reused across
    /// connections without rebuilding SQL.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::InvalidQuery`] for a zero or excessive limit,
    /// invalid field metadata, duplicate ordering, or an excessive predicate.
    pub fn take(self, limit: u32) -> Result<CollectionQuery<C>> {
        compile_query::<C>(self.predicate.as_ref(), &self.order, limit)
    }
}

impl<C: Collection> Default for QueryDraft<C> {
    fn default() -> Self {
        Self::new()
    }
}

/// An immutable, bounded, executable typed collection query.
pub struct CollectionQuery<C> {
    sql: String,
    values: Vec<Value>,
    limit: u32,
    marker: PhantomData<fn() -> C>,
}

impl<C: Collection> CollectionQuery<C> {
    #[must_use]
    pub const fn limit(&self) -> u32 {
        self.limit
    }

    #[must_use]
    pub fn collection(&self) -> &'static crate::CollectionSchema {
        C::schema()
    }
}

impl<C> Clone for CollectionQuery<C> {
    fn clone(&self) -> Self {
        Self {
            sql: self.sql.clone(),
            values: self.values.clone(),
            limit: self.limit,
            marker: PhantomData,
        }
    }
}

impl<C: Collection> std::fmt::Debug for CollectionQuery<C> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CollectionQuery")
            .field("collection_id", &C::schema().id())
            .field("limit", &self.limit)
            .field("bound_values", &self.values.len())
            .finish_non_exhaustive()
    }
}

/// Executes bounded typed collection queries on a compatible connection.
pub trait CollectionQueryExecutor {
    /// Executes a finalized query and decodes at most its declared limit.
    ///
    /// # Errors
    ///
    /// Returns an error for statement compilation, binding, execution, or row
    /// decoding failures.
    fn query_collection<C: Collection>(&self, query: &CollectionQuery<C>) -> Result<Vec<C>>;
}

impl CollectionQueryExecutor for Database {
    fn query_collection<C: Collection>(&self, query: &CollectionQuery<C>) -> Result<Vec<C>> {
        execute_query(self.prepare_cached(&query.sql)?, query)
    }
}

impl CollectionQueryExecutor for Transaction<'_> {
    fn query_collection<C: Collection>(&self, query: &CollectionQuery<C>) -> Result<Vec<C>> {
        execute_query(self.prepare(&query.sql)?, query)
    }
}

impl CollectionQueryExecutor for ReadConnection<'_> {
    fn query_collection<C: Collection>(&self, query: &CollectionQuery<C>) -> Result<Vec<C>> {
        execute_query(self.prepare_cached(&query.sql)?, query)
    }
}

impl CollectionQueryExecutor for ReadTransaction<'_> {
    fn query_collection<C: Collection>(&self, query: &CollectionQuery<C>) -> Result<Vec<C>> {
        execute_query(self.prepare(&query.sql)?, query)
    }
}

impl Database {
    /// Executes a finalized typed collection query.
    ///
    /// # Errors
    ///
    /// Returns an error for statement compilation, execution, or row decoding.
    pub fn query_collection<C: Collection>(&self, query: &CollectionQuery<C>) -> Result<Vec<C>> {
        CollectionQueryExecutor::query_collection(self, query)
    }
}

impl DatabasePool {
    /// Executes a finalized typed collection query on a pooled reader.
    ///
    /// # Errors
    ///
    /// Returns an error for reader checkout, execution, or row decoding.
    pub fn query_collection<C: Collection>(&self, query: &CollectionQuery<C>) -> Result<Vec<C>> {
        self.read(|connection| connection.query_collection(query))
    }
}

impl ReadConnection<'_> {
    /// Executes a finalized typed collection query on this reader.
    ///
    /// # Errors
    ///
    /// Returns an error for statement compilation, execution, or row decoding.
    pub fn query_collection<C: Collection>(&self, query: &CollectionQuery<C>) -> Result<Vec<C>> {
        CollectionQueryExecutor::query_collection(self, query)
    }
}

impl ReadTransaction<'_> {
    /// Executes a finalized typed collection query in the current snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error for statement compilation, execution, or row decoding.
    pub fn query_collection<C: Collection>(&self, query: &CollectionQuery<C>) -> Result<Vec<C>> {
        CollectionQueryExecutor::query_collection(self, query)
    }
}

fn compile_query<C: Collection>(
    predicate: Option<&PredicateNode>,
    order: &[QueryOrder<C>],
    limit: u32,
) -> Result<CollectionQuery<C>> {
    if limit == 0 || limit > MAX_QUERY_LIMIT || order.len() > MAX_QUERY_ORDER_FIELDS {
        return Err(invalid_query());
    }

    let schema = C::schema();
    validate_collection_schema(schema).map_err(|_| invalid_query())?;
    let primary_key = schema.primary_key().ok_or_else(invalid_query)?;
    let mut sql = String::from("SELECT ");
    for (index, field) in schema.fields().iter().enumerate() {
        if index != 0 {
            sql.push_str(", ");
        }
        push_identifier(&mut sql, field.column());
    }
    sql.push_str(" FROM ");
    push_identifier(&mut sql, schema.table());

    let mut values = Vec::new();
    if let Some(predicate) = predicate {
        sql.push_str(" WHERE ");
        let mut nodes = 0;
        compile_predicate::<C>(predicate, &mut sql, &mut values, 1, &mut nodes)?;
    }

    sql.push_str(" ORDER BY ");
    let mut ordered_fields = HashSet::with_capacity(order.len() + 1);
    for (index, term) in order.iter().enumerate() {
        validate_field::<C>(term.field)?;
        if !ordered_fields.insert(term.field.id()) {
            return Err(invalid_query());
        }
        if index != 0 {
            sql.push_str(", ");
        }
        push_order(&mut sql, term.field, term.direction);
    }
    if !ordered_fields.contains(primary_key.id()) {
        if !order.is_empty() {
            sql.push_str(", ");
        }
        push_order(&mut sql, primary_key, OrderDirection::Ascending);
    }

    sql.push_str(" LIMIT ?");
    values.push(Value::Integer(i64::from(limit)));
    Ok(CollectionQuery {
        sql,
        values,
        limit,
        marker: PhantomData,
    })
}

fn compile_predicate<C: Collection>(
    predicate: &PredicateNode,
    sql: &mut String,
    values: &mut Vec<Value>,
    depth: usize,
    nodes: &mut usize,
) -> Result<()> {
    *nodes = nodes.checked_add(1).ok_or_else(invalid_query)?;
    if depth > MAX_PREDICATE_DEPTH || *nodes > MAX_PREDICATE_NODES {
        return Err(invalid_query());
    }
    match predicate {
        PredicateNode::Compare {
            field,
            comparison,
            value,
        } => {
            validate_field::<C>(field)?;
            validate_bound_value(field, value)?;
            sql.push('(');
            push_identifier(sql, field.column());
            if matches!(value, Value::Null) {
                match comparison {
                    Comparison::Equal => sql.push_str(" IS NULL"),
                    Comparison::NotEqual => sql.push_str(" IS NOT NULL"),
                    Comparison::Less
                    | Comparison::LessOrEqual
                    | Comparison::Greater
                    | Comparison::GreaterOrEqual => return Err(invalid_query()),
                }
            } else {
                sql.push_str(match comparison {
                    Comparison::Equal => " = ?",
                    Comparison::NotEqual => " <> ?",
                    Comparison::Less => " < ?",
                    Comparison::LessOrEqual => " <= ?",
                    Comparison::Greater => " > ?",
                    Comparison::GreaterOrEqual => " >= ?",
                });
                values.push(value.clone());
            }
            sql.push(')');
        }
        PredicateNode::Null { field, comparison } => {
            validate_field::<C>(field)?;
            if !field.is_nullable() {
                return Err(invalid_query());
            }
            sql.push('(');
            push_identifier(sql, field.column());
            sql.push_str(match comparison {
                NullComparison::IsNull => " IS NULL",
                NullComparison::IsNotNull => " IS NOT NULL",
            });
            sql.push(')');
        }
        PredicateNode::And(left, right) | PredicateNode::Or(left, right) => {
            sql.push('(');
            compile_predicate::<C>(left, sql, values, depth + 1, nodes)?;
            if matches!(predicate, PredicateNode::And(_, _)) {
                sql.push_str(" AND ");
            } else {
                sql.push_str(" OR ");
            }
            compile_predicate::<C>(right, sql, values, depth + 1, nodes)?;
            sql.push(')');
        }
        PredicateNode::Not(inner) => {
            sql.push_str("(NOT ");
            compile_predicate::<C>(inner, sql, values, depth + 1, nodes)?;
            sql.push(')');
        }
    }
    Ok(())
}

fn validate_field<C: Collection>(field: &FieldSchema) -> Result<()> {
    if C::schema()
        .fields()
        .iter()
        .any(|candidate| candidate == field)
    {
        Ok(())
    } else {
        Err(invalid_query())
    }
}

fn validate_bound_value(field: &FieldSchema, value: &Value) -> Result<()> {
    let valid = match value {
        Value::Null => field.is_nullable(),
        Value::Integer(_) => field.column_type() == ColumnType::Integer,
        Value::Real(value) => field.column_type() == ColumnType::Real && value.is_finite(),
        Value::Text(_) => field.column_type() == ColumnType::Text,
        Value::Blob(_) => field.column_type() == ColumnType::Blob,
    };
    if valid {
        Ok(())
    } else {
        Err(invalid_query())
    }
}

fn push_order(sql: &mut String, field: &FieldSchema, direction: OrderDirection) {
    push_identifier(sql, field.column());
    sql.push_str(match direction {
        OrderDirection::Ascending => " ASC",
        OrderDirection::Descending => " DESC",
    });
}

fn push_identifier(sql: &mut String, identifier: &str) {
    sql.push('"');
    for character in identifier.chars() {
        if character == '"' {
            sql.push('"');
        }
        sql.push(character);
    }
    sql.push('"');
}

fn execute_query<C: Collection>(
    mut statement: Statement<'_>,
    query: &CollectionQuery<C>,
) -> Result<Vec<C>> {
    let references = query.values.iter().map(Value::as_ref).collect::<Vec<_>>();
    let mut rows = statement.query(&references)?;
    let capacity = usize::try_from(query.limit).map_err(|_| invalid_query())?;
    let mut records = Vec::with_capacity(capacity);
    while let Some(row) = rows.next_row()? {
        records.push(C::from_row(&row)?);
    }
    Ok(records)
}

fn invalid_query() -> Error {
    Error::new(
        ErrorCode::InvalidQuery,
        "typed collection query is invalid or exceeds its safety limits",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PoolOptions, SchemaRegistration};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DATABASE: AtomicU64 = AtomicU64::new(1);
    static FORGED: FieldSchema = FieldSchema::new(
        "agent.query-memory.namespace",
        "forged_column",
        ColumnType::Text,
        false,
        false,
    );

    struct TestDatabasePath(PathBuf);

    impl TestDatabasePath {
        fn new(name: &str) -> Self {
            let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
            Self(std::env::temp_dir().join(format!(
                "csgdb-query-{name}-{}-{sequence}.db",
                std::process::id()
            )))
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDatabasePath {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
            let base = self.0.to_string_lossy();
            let _ = fs::remove_file(format!("{base}-wal"));
            let _ = fs::remove_file(format!("{base}-shm"));
        }
    }

    #[derive(Clone, Debug, PartialEq, crate::Collection)]
    #[csgdb(collection = "agent.query-memory", table = "query_memory", version = 1)]
    struct QueryMemory {
        #[csgdb(id = "agent.query-memory.id", column = "id", primary_key)]
        id: i64,
        #[csgdb(id = "agent.query-memory.namespace", column = "namespace")]
        namespace: String,
        #[csgdb(id = "agent.query-memory.score", column = "score")]
        score: Option<f64>,
        #[csgdb(id = "agent.query-memory.active", column = "active")]
        active: bool,
    }

    fn memory(id: i64, namespace: &str, score: Option<f64>, active: bool) -> QueryMemory {
        QueryMemory {
            id,
            namespace: namespace.to_owned(),
            score,
            active,
        }
    }

    #[test]
    fn structured_predicates_bind_values_and_order_deterministically() {
        let path = TestDatabasePath::new("predicates");
        let mut database =
            Database::open_with_passphrase(path.path(), "query-secret").expect("open");
        database
            .register_collection::<QueryMemory>()
            .expect("register");
        for record in [
            memory(2, "session", Some(0.9), true),
            memory(1, "session", Some(0.9), true),
            memory(3, "session", Some(0.7), true),
            memory(4, "other", Some(0.99), true),
            memory(5, "session", None, true),
            memory(6, "session", Some(0.8), false),
        ] {
            database.insert(&record).expect("insert");
        }

        let predicate = QueryMemory::FIELD_NAMESPACE
            .eq("session")
            .expect("text predicate")
            .and(
                QueryMemory::FIELD_SCORE
                    .ge(Some(0.7))
                    .expect("score predicate"),
            )
            .and(
                QueryMemory::FIELD_ACTIVE
                    .eq(true)
                    .expect("active predicate"),
            );
        let query = QueryMemory::query()
            .filter(predicate)
            .order_by(QueryMemory::FIELD_SCORE.desc())
            .take(3)
            .expect("finalize query");
        assert_eq!(query.limit(), 3);
        assert_eq!(
            database
                .query_collection(&query)
                .expect("execute")
                .iter()
                .map(|memory| memory.id)
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );

        let null_query = QueryMemory::query()
            .filter(QueryMemory::FIELD_SCORE.is_null())
            .take(10)
            .expect("null query");
        assert_eq!(
            database
                .query_collection(&null_query)
                .expect("execute null query")
                .iter()
                .map(|memory| memory.id)
                .collect::<Vec<_>>(),
            [5]
        );

        let non_null_query = QueryMemory::query()
            .filter(QueryMemory::FIELD_SCORE.ne(None).expect("not-null query"))
            .take(10)
            .expect("finalize not-null query");
        assert_eq!(
            database
                .query_collection(&non_null_query)
                .expect("execute not-null query")
                .len(),
            5
        );

        let logic_query = QueryMemory::query()
            .filter(
                QueryMemory::FIELD_NAMESPACE
                    .eq("other")
                    .expect("other")
                    .or(QueryMemory::FIELD_ACTIVE.eq(false).expect("inactive"))
                    .not(),
            )
            .take(10)
            .expect("logical query");
        assert_eq!(
            database
                .query_collection(&logic_query)
                .expect("logic")
                .len(),
            4
        );
    }

    #[test]
    fn bound_text_cannot_change_query_structure() {
        let path = TestDatabasePath::new("binding");
        let mut database =
            Database::open_with_passphrase(path.path(), "binding-secret").expect("open");
        database
            .register_collection::<QueryMemory>()
            .expect("register");
        database
            .insert(&memory(1, "safe", Some(1.0), true))
            .expect("insert");
        let hostile = "safe\" OR 1=1; DROP TABLE query_memory; --";
        let query = QueryMemory::query()
            .filter(QueryMemory::FIELD_NAMESPACE.eq(hostile).expect("predicate"))
            .take(5)
            .expect("query");
        assert!(database
            .query_collection(&query)
            .expect("execute")
            .is_empty());
        assert_eq!(
            database
                .get::<QueryMemory>(&1)
                .expect("table survives")
                .unwrap()
                .id,
            1
        );
    }

    #[test]
    fn invalid_limits_metadata_and_complexity_are_rejected() {
        assert_eq!(
            QueryMemory::query().take(0).expect_err("zero limit").code(),
            ErrorCode::InvalidQuery
        );
        assert_eq!(
            QueryMemory::query()
                .take(MAX_QUERY_LIMIT + 1)
                .expect_err("large limit")
                .code(),
            ErrorCode::InvalidQuery
        );
        assert_eq!(
            QueryMemory::query()
                .order_by(QueryMemory::FIELD_ID.asc())
                .order_by(QueryMemory::FIELD_ID.desc())
                .take(1)
                .expect_err("duplicate order")
                .code(),
            ErrorCode::InvalidQuery
        );

        let forged = CollectionField::<QueryMemory, String>::new(&FORGED);
        assert_eq!(
            QueryMemory::query()
                .filter(forged.eq("safe").expect("encode"))
                .take(1)
                .expect_err("forged metadata")
                .code(),
            ErrorCode::InvalidQuery
        );

        let mut deep = QueryMemory::FIELD_ACTIVE.eq(true).expect("predicate");
        for _ in 0..MAX_PREDICATE_DEPTH {
            deep = deep.not();
        }
        assert_eq!(
            QueryMemory::query()
                .filter(deep)
                .take(1)
                .expect_err("deep predicate")
                .code(),
            ErrorCode::InvalidQuery
        );

        let mut wide = QueryMemory::FIELD_ACTIVE.eq(true).expect("predicate");
        for _ in 0..8 {
            wide = wide.clone().and(wide);
        }
        assert_eq!(
            QueryMemory::query()
                .filter(wide)
                .take(1)
                .expect_err("wide predicate")
                .code(),
            ErrorCode::InvalidQuery
        );

        assert_eq!(
            QueryMemory::query()
                .filter(
                    QueryMemory::FIELD_SCORE
                        .gt(None)
                        .expect("encode null comparison"),
                )
                .take(1)
                .expect_err("ordered null")
                .code(),
            ErrorCode::InvalidQuery
        );
    }

    #[test]
    fn queries_execute_on_transactions_and_pooled_snapshots() {
        let path = TestDatabasePath::new("executors");
        let mut database =
            Database::open_with_passphrase(path.path(), "executor-secret").expect("open");
        assert_eq!(
            database
                .register_collection::<QueryMemory>()
                .expect("register"),
            SchemaRegistration::Created
        );
        let query = QueryMemory::query()
            .filter(
                QueryMemory::FIELD_ACTIVE.eq(true).expect("predicate").and(
                    QueryMemory::FIELD_NAMESPACE
                        .eq("transaction")
                        .expect("private bound value"),
                ),
            )
            .take(8)
            .expect("query");
        assert!(!format!("{query:?}").contains("transaction"));
        {
            let transaction = database.transaction().expect("transaction");
            crate::CollectionCrud::insert(&transaction, &memory(1, "transaction", Some(0.5), true))
                .expect("insert");
            assert_eq!(
                CollectionQueryExecutor::query_collection(&transaction, &query)
                    .expect("transaction query")
                    .len(),
                1
            );
            transaction.commit().expect("commit");
        }
        database.close().expect("close");

        let pool = DatabasePool::open_with_passphrase(
            path.path(),
            "executor-secret",
            PoolOptions::default(),
        )
        .expect("open pool");
        assert_eq!(pool.query_collection(&query).expect("pool query").len(), 1);
        pool.read(|connection| {
            assert_eq!(connection.query_collection(&query)?.len(), 1);
            let snapshot = connection.transaction()?;
            assert_eq!(snapshot.query_collection(&query)?.len(), 1);
            snapshot.commit()
        })
        .expect("reader paths");
        pool.close().expect("close pool");
    }
}
