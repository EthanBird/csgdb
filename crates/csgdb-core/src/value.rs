/// The storage class of a database value.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(i32)]
pub enum ValueType {
    Integer = 1,
    Real = 2,
    Text = 3,
    Blob = 4,
    Null = 5,
}

/// An owned dynamically typed database value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl Value {
    #[must_use]
    pub const fn value_type(&self) -> ValueType {
        match self {
            Self::Null => ValueType::Null,
            Self::Integer(_) => ValueType::Integer,
            Self::Real(_) => ValueType::Real,
            Self::Text(_) => ValueType::Text,
            Self::Blob(_) => ValueType::Blob,
        }
    }

    #[must_use]
    pub fn as_ref(&self) -> ValueRef<'_> {
        match self {
            Self::Null => ValueRef::Null,
            Self::Integer(value) => ValueRef::Integer(*value),
            Self::Real(value) => ValueRef::Real(*value),
            Self::Text(value) => ValueRef::Text(value),
            Self::Blob(value) => ValueRef::Blob(value),
        }
    }
}

/// A borrowed dynamically typed database value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ValueRef<'value> {
    Null,
    Integer(i64),
    Real(f64),
    Text(&'value str),
    Blob(&'value [u8]),
}

impl ValueRef<'_> {
    #[must_use]
    pub const fn value_type(self) -> ValueType {
        match self {
            Self::Null => ValueType::Null,
            Self::Integer(_) => ValueType::Integer,
            Self::Real(_) => ValueType::Real,
            Self::Text(_) => ValueType::Text,
            Self::Blob(_) => ValueType::Blob,
        }
    }

    #[must_use]
    pub fn to_owned(self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Integer(value) => Value::Integer(value),
            Self::Real(value) => Value::Real(value),
            Self::Text(value) => Value::Text(value.to_owned()),
            Self::Blob(value) => Value::Blob(value.to_owned()),
        }
    }
}

impl From<i64> for Value {
    fn from(value: i64) -> Self {
        Self::Integer(value)
    }
}

impl From<f64> for Value {
    fn from(value: f64) -> Self {
        Self::Real(value)
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Self::Text(value.to_owned())
    }
}

impl From<Vec<u8>> for Value {
    fn from(value: Vec<u8>) -> Self {
        Self::Blob(value)
    }
}

impl From<&[u8]> for Value {
    fn from(value: &[u8]) -> Self {
        Self::Blob(value.to_owned())
    }
}

impl From<i64> for ValueRef<'_> {
    fn from(value: i64) -> Self {
        Self::Integer(value)
    }
}

impl From<f64> for ValueRef<'_> {
    fn from(value: f64) -> Self {
        Self::Real(value)
    }
}

impl<'value> From<&'value str> for ValueRef<'value> {
    fn from(value: &'value str) -> Self {
        Self::Text(value)
    }
}

impl<'value> From<&'value [u8]> for ValueRef<'value> {
    fn from(value: &'value [u8]) -> Self {
        Self::Blob(value)
    }
}

impl<'value> From<&'value Value> for ValueRef<'value> {
    fn from(value: &'value Value) -> Self {
        value.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn borrowed_values_round_trip_to_owned_values() {
        let values = [
            Value::Null,
            Value::Integer(42),
            Value::Real(3.5),
            Value::Text("memory".to_owned()),
            Value::Blob(vec![0, 1, 255]),
        ];

        for value in values {
            assert_eq!(value.as_ref().to_owned(), value);
            assert_eq!(value.as_ref().value_type(), value.value_type());
        }
    }
}
