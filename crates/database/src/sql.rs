//! Typed parameters and checked result decoding for Toasty's native SQL API.
use crate::{Error, Result};
use chrono::{DateTime, Utc};
use toasty::{
    db::Executor,
    schema::db,
    stmt::{Type, Value},
};
use uuid::Uuid;

pub struct Parameter {
    value: Value,
    ty: db::Type,
}
pub type Parameters = Vec<Result<Parameter>>;

pub trait Encode {
    fn storage_type() -> db::Type;
    fn encode(&self) -> Result<Value>;
}
pub fn parameter<T: Encode + ?Sized>(value: &T) -> Result<Parameter> {
    Ok(Parameter {
        value: value.encode()?,
        ty: T::storage_type(),
    })
}
#[macro_export]
macro_rules! params {
    ($($value:expr),* $(,)?) => { vec![$($crate::sql::parameter(&$value)),*] };
}
impl<T: Encode + ?Sized> Encode for &T {
    fn storage_type() -> db::Type {
        T::storage_type()
    }
    fn encode(&self) -> Result<Value> {
        T::encode(self)
    }
}
impl<T: Encode> Encode for Option<T> {
    fn storage_type() -> db::Type {
        T::storage_type()
    }
    fn encode(&self) -> Result<Value> {
        self.as_ref().map_or(Ok(Value::Null), Encode::encode)
    }
}
impl<T: Encode> Encode for [T] {
    fn storage_type() -> db::Type {
        db::Type::List(Box::new(T::storage_type()))
    }
    fn encode(&self) -> Result<Value> {
        self.iter()
            .map(Encode::encode)
            .collect::<Result<Vec<_>>>()
            .map(Value::List)
    }
}
impl<T: Encode> Encode for Vec<T> {
    fn storage_type() -> db::Type {
        <[T]>::storage_type()
    }
    fn encode(&self) -> Result<Value> {
        self.as_slice().encode()
    }
}
impl<T: Encode, const N: usize> Encode for [T; N] {
    fn storage_type() -> db::Type {
        <[T]>::storage_type()
    }
    fn encode(&self) -> Result<Value> {
        self.as_slice().encode()
    }
}
macro_rules! primitive {
    ($ty:ty, $storage:expr, $variant:ident) => {
        impl Encode for $ty {
            fn storage_type() -> db::Type {
                $storage
            }
            fn encode(&self) -> Result<Value> {
                Ok(Value::$variant(*self))
            }
        }
        impl Decode for $ty {
            fn value_type() -> Type {
                Type::$variant
            }
            fn decode(value: &Value) -> Result<Self> {
                if let Value::$variant(value) = value {
                    Ok(*value)
                } else {
                    Err(Error::Decode("unexpected database type"))
                }
            }
        }
    };
}
primitive!(bool, db::Type::Boolean, Bool);
primitive!(i16, db::Type::Integer(2), I16);
primitive!(i32, db::Type::Integer(4), I32);
primitive!(i64, db::Type::Integer(8), I64);
primitive!(f32, db::Type::Float(4), F32);
primitive!(f64, db::Type::Float(8), F64);
primitive!(Uuid, db::Type::Uuid, Uuid);
impl Encode for str {
    fn storage_type() -> db::Type {
        db::Type::Text
    }
    fn encode(&self) -> Result<Value> {
        Ok(Value::String(self.to_owned()))
    }
}
impl Encode for String {
    fn storage_type() -> db::Type {
        db::Type::Text
    }
    fn encode(&self) -> Result<Value> {
        self.as_str().encode()
    }
}
impl Encode for serde_json::Value {
    fn storage_type() -> db::Type {
        db::Type::Jsonb
    }
    fn encode(&self) -> Result<Value> {
        Ok(Value::String(self.to_string()))
    }
}
/// PostgreSQL stores microseconds. Convert once at the persistence boundary in UTC.
pub fn timestamp(value: DateTime<Utc>) -> Result<jiff::Timestamp> {
    jiff::Timestamp::new(
        value.timestamp(),
        (value.timestamp_subsec_micros() * 1000) as i32,
    )
    .map_err(|_| Error::Decode("timestamp outside database range"))
}
pub fn chrono(value: jiff::Timestamp) -> Result<DateTime<Utc>> {
    DateTime::from_timestamp_micros(value.as_microsecond())
        .ok_or(Error::Decode("timestamp outside API range"))
}
impl Encode for DateTime<Utc> {
    fn storage_type() -> db::Type {
        db::Type::Timestamp(6)
    }
    fn encode(&self) -> Result<Value> {
        timestamp(*self).map(Value::Timestamp)
    }
}

pub trait Decode: Sized {
    fn value_type() -> Type;
    fn decode(value: &Value) -> Result<Self>;
}
impl Decode for String {
    fn value_type() -> Type {
        Type::String
    }
    fn decode(value: &Value) -> Result<Self> {
        if let Value::String(v) = value {
            Ok(v.clone())
        } else {
            Err(Error::Decode("expected database text"))
        }
    }
}
impl Decode for serde_json::Value {
    fn value_type() -> Type {
        Type::String
    }
    fn decode(value: &Value) -> Result<Self> {
        if let Value::String(v) = value {
            serde_json::from_str(v).map_err(|_| Error::Decode("invalid database JSON"))
        } else {
            Err(Error::Decode("expected database JSON"))
        }
    }
}
impl Decode for DateTime<Utc> {
    fn value_type() -> Type {
        Type::Timestamp
    }
    fn decode(value: &Value) -> Result<Self> {
        if let Value::Timestamp(v) = value {
            chrono(*v)
        } else {
            Err(Error::Decode("expected database timestamp"))
        }
    }
}
impl Decode for jiff::Timestamp {
    fn value_type() -> Type {
        Type::Timestamp
    }
    fn decode(value: &Value) -> Result<Self> {
        if let Value::Timestamp(value) = value {
            Ok(*value)
        } else {
            Err(Error::Decode("expected database timestamp"))
        }
    }
}
impl<T: Decode> Decode for Option<T> {
    fn value_type() -> Type {
        T::value_type()
    }
    fn decode(value: &Value) -> Result<Self> {
        if value.is_null() {
            Ok(None)
        } else {
            T::decode(value).map(Some)
        }
    }
}
impl<T: Decode> Decode for Vec<T> {
    fn value_type() -> Type {
        Type::List(Box::new(T::value_type()))
    }
    fn decode(value: &Value) -> Result<Self> {
        if let Value::List(v) = value {
            v.iter().map(T::decode).collect()
        } else {
            Err(Error::Decode("expected database array"))
        }
    }
}

/// Private to persistence callers; column descriptions accompany native SQL projections.
#[derive(Debug, Clone)]
pub struct Record {
    values: Vec<Value>,
    names: Vec<&'static str>,
}
impl Record {
    pub fn decode<T: Decode>(&self, name: &str) -> Result<T> {
        let index = self
            .names
            .iter()
            .position(|n| *n == name)
            .ok_or(Error::Decode("unknown result column"))?;
        T::decode(&self.values[index])
    }
}
fn fields(value: Value) -> Result<Vec<Value>> {
    if let Value::Record(row) = value {
        Ok(row.fields)
    } else {
        Err(Error::Decode("expected result record"))
    }
}
fn exactly_one<T>(mut values: Vec<T>) -> Result<T> {
    if values.len() != 1 {
        return Err(if values.is_empty() {
            Error::NotFound
        } else {
            Error::Decode("expected one result record")
        });
    }
    Ok(values.remove(0))
}
fn at_most_one<T>(mut values: Vec<T>) -> Result<Option<T>> {
    if values.len() > 1 {
        return Err(Error::Decode("expected at most one result record"));
    }
    Ok(values.pop())
}
pub async fn execute(
    executor: &mut dyn Executor,
    text: &str,
    parameters: Parameters,
) -> Result<u64> {
    let mut stmt = toasty::sql::statement(text);
    for parameter in parameters {
        let parameter = parameter?;
        stmt = stmt.bind_typed(parameter.value, parameter.ty);
    }
    Ok(stmt.exec(executor).await?)
}
async fn query(
    executor: &mut dyn Executor,
    text: &str,
    parameters: Parameters,
    types: impl IntoIterator<Item = Type>,
) -> Result<Vec<Value>> {
    let mut query = toasty::sql::query(text).column_types(types);
    for parameter in parameters {
        let parameter = parameter?;
        query = query.bind_typed(parameter.value, parameter.ty);
    }
    Ok(query.exec(executor).await?)
}
pub async fn scalars<T: Decode>(
    executor: &mut dyn Executor,
    text: &str,
    parameters: Parameters,
) -> Result<Vec<T>> {
    query(executor, text, parameters, [T::value_type()])
        .await?
        .into_iter()
        .map(|row| T::decode(&exactly_one(fields(row)?)?))
        .collect()
}

/// A single statement snapshot, decoded in pages of at most 128 rows.
///
/// Owns the physical connection until completion. Abandoning the reader on a
/// response limit, error or cancellation closes that connection rather than
/// draining the rest of a potentially large response in the background.
pub struct ScalarPages<T> {
    connection: Option<toasty::Connection>,
    stream: Option<toasty_core::stmt::ValueStream>,
    value: std::marker::PhantomData<fn() -> T>,
}

pub async fn scalar_pages<T: Decode>(
    db: &toasty::Db,
    text: &str,
    parameters: Parameters,
) -> Result<ScalarPages<T>> {
    let mut query = toasty::sql::query(text).column_types([T::value_type()]);
    for parameter in parameters {
        let parameter = parameter?;
        query = query.bind_typed(parameter.value, parameter.ty);
    }
    // Install the drop guard before awaiting the driver, including prepare and
    // the first row, so cancellation there also closes the physical connection.
    let mut pages = ScalarPages {
        connection: Some(db.connection().await?),
        stream: None,
        value: std::marker::PhantomData,
    };
    pages.stream = Some(
        query
            .stream(
                pages
                    .connection
                    .as_mut()
                    .expect("connection is held until EOF"),
            )
            .await?,
    );
    Ok(pages)
}

impl<T: Decode> ScalarPages<T> {
    pub async fn next_page(&mut self) -> Result<Vec<T>> {
        let mut page = Vec::with_capacity(128);
        if let Some(stream) = &mut self.stream {
            for _ in 0..128 {
                match stream.next().await {
                    Some(row) => page.push(T::decode(&exactly_one(fields(row?)?)?)?),
                    None => {
                        self.stream = None;
                        // EOF includes PostgreSQL's command completion. This
                        // connection can now be safely returned to the pool.
                        self.connection = None;
                        break;
                    }
                }
            }
        }
        Ok(page)
    }
}

impl<T> Drop for ScalarPages<T> {
    fn drop(&mut self) {
        self.stream = None;
        if let Some(connection) = self.connection.take() {
            connection.discard();
        }
    }
}

pub async fn scalar<T: Decode>(
    executor: &mut dyn Executor,
    text: &str,
    parameters: Parameters,
) -> Result<T> {
    exactly_one(scalars(executor, text, parameters).await?)
}
pub async fn optional_scalar<T: Decode>(
    executor: &mut dyn Executor,
    text: &str,
    parameters: Parameters,
) -> Result<Option<T>> {
    at_most_one(scalars(executor, text, parameters).await?)
}
pub async fn records(
    executor: &mut dyn Executor,
    text: &str,
    parameters: Parameters,
    columns: &[(&'static str, Type)],
) -> Result<Vec<Record>> {
    query(
        executor,
        text,
        parameters,
        columns.iter().map(|(_, ty)| ty.clone()),
    )
    .await?
    .into_iter()
    .map(|row| {
        let values = fields(row)?;
        if values.len() != columns.len() {
            return Err(Error::Decode("result projection changed"));
        }
        Ok(Record {
            values,
            names: columns.iter().map(|(name, _)| *name).collect(),
        })
    })
    .collect()
}
pub async fn record(
    executor: &mut dyn Executor,
    text: &str,
    parameters: Parameters,
    columns: &[(&'static str, Type)],
) -> Result<Record> {
    exactly_one(records(executor, text, parameters, columns).await?)
}
pub async fn optional_record(
    executor: &mut dyn Executor,
    text: &str,
    parameters: Parameters,
    columns: &[(&'static str, Type)],
) -> Result<Option<Record>> {
    at_most_one(records(executor, text, parameters, columns).await?)
}
pub trait DecodeTuple: Sized {
    fn types() -> Vec<Type>;
    fn decode(values: Vec<Value>) -> Result<Self>;
}
macro_rules! tuple_impl {
    ($($ty:ident),+) => {
        impl<$($ty:Decode),+> DecodeTuple for ($($ty,)+) {
            fn types() -> Vec<Type> {vec![$($ty::value_type()),+]}
            fn decode(values:Vec<Value>) -> Result<Self> {
                let mut values=values.into_iter();
                let result=($($ty::decode(&values.next().ok_or(Error::Decode("missing tuple field"))?)?,)+);
                if values.next().is_some() {return Err(Error::Decode("extra tuple field"));}
                Ok(result)
            }
        }
    }
}
tuple_impl!(A, B);
tuple_impl!(A, B, C);
tuple_impl!(A, B, C, D);
pub async fn tuples<T: DecodeTuple>(
    executor: &mut dyn Executor,
    text: &str,
    parameters: Parameters,
) -> Result<Vec<T>> {
    query(executor, text, parameters, T::types())
        .await?
        .into_iter()
        .map(|row| T::decode(fields(row)?))
        .collect()
}
pub async fn tuple<T: DecodeTuple>(
    executor: &mut dyn Executor,
    text: &str,
    parameters: Parameters,
) -> Result<T> {
    exactly_one(tuples(executor, text, parameters).await?)
}
