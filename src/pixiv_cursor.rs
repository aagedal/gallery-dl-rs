//! A typed Pixiv bookmark cursor preserving its JSON scalar representation.
use serde::{Deserialize, Deserializer, Serialize, de};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct BookmarkCursor(Value);

impl BookmarkCursor {
    pub fn parse(value: &Value) -> Option<Self> {
        let valid = match value {
            Value::Null => true,
            Value::String(value) => value.len() <= 1024,
            Value::Number(value) => {
                let value = value.as_str();
                value.len() <= 1024 && !value.contains(['.', 'e', 'E'])
            }
            _ => false,
        };
        valid.then(|| {
            // Python's JSON integer parser canonicalizes -0 before requests
            // serializes it; strings such as "-0" retain their exact spelling.
            Self(
                if value
                    .as_number()
                    .is_some_and(|number| number.as_str() == "-0")
                {
                    Value::from(0)
                } else {
                    value.clone()
                },
            )
        })
    }

    /// Null clears an inherited cursor and omits the API query parameter.
    pub fn query_value(&self) -> Option<&str> {
        match &self.0 {
            Value::Null => None,
            Value::String(value) => Some(value),
            Value::Number(value) => Some(value.as_str()),
            _ => unreachable!("validated bookmark cursor"),
        }
    }

    pub fn as_json(&self) -> &Value {
        &self.0
    }
}

impl<'de> Deserialize<'de> for BookmarkCursor {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let invalid = || de::Error::custom("invalid Pixiv bookmark cursor");
        let value = Value::deserialize(deserializer).map_err(|_| invalid())?;
        Self::parse(&value).ok_or_else(invalid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scalar_representation_and_query_values_round_trip_without_conflation() {
        let negative_zero: BookmarkCursor = serde_json::from_str("-0").unwrap();
        assert_eq!(negative_zero.query_value(), Some("0"));
        assert_eq!(serde_json::to_string(&negative_zero).unwrap(), "0");
        assert_eq!(
            BookmarkCursor::parse(&json!("-0")).unwrap().query_value(),
            Some("-0")
        );
        for (value, query) in [
            (Value::Null, None),
            (json!(""), Some("")),
            (json!("0"), Some("0")),
            (json!(0), Some("0")),
            (json!(-12), Some("-12")),
            (json!("猫 \t\n"), Some("猫 \t\n")),
        ] {
            let cursor = BookmarkCursor::parse(&value).unwrap();
            assert_eq!(cursor.query_value(), query);
            assert_eq!(cursor.as_json(), &value);
            assert_eq!(serde_json::to_value(&cursor).unwrap(), value);
            assert_eq!(
                serde_json::from_value::<BookmarkCursor>(value).unwrap(),
                cursor
            );
        }
        assert_ne!(
            BookmarkCursor::parse(&json!(0)),
            BookmarkCursor::parse(&json!("0"))
        );
        assert_ne!(
            BookmarkCursor::parse(&Value::Null),
            BookmarkCursor::parse(&json!(""))
        );
        let digits = "9".repeat(1024);
        let cursor: BookmarkCursor = serde_json::from_str(&digits).unwrap();
        assert_eq!(cursor.query_value(), Some(digits.as_str()));
        assert_eq!(serde_json::to_string(&cursor).unwrap(), digits);
        assert!(BookmarkCursor::parse(&json!("猫".repeat(341))).is_some());
        assert!(BookmarkCursor::parse(&json!("猫".repeat(342))).is_none());
    }

    #[test]
    fn invalid_cursors_are_rejected_with_payload_free_diagnostics() {
        for value in [
            json!(true),
            json!(false),
            json!(1.5),
            json!([]),
            json!(["cursor-private-payload"]),
            json!({"private":"cursor-private-payload"}),
            json!("x".repeat(1025)),
        ] {
            assert!(BookmarkCursor::parse(&value).is_none());
            let message = serde_json::from_value::<BookmarkCursor>(value)
                .unwrap_err()
                .to_string();
            assert_eq!(message, "invalid Pixiv bookmark cursor");
        }
        for number in ["1e10", "1E10", "1.0", "-1.2", &"9".repeat(1025)] {
            let value: Value = serde_json::from_str(number).unwrap();
            assert!(BookmarkCursor::parse(&value).is_none());
            assert_eq!(
                serde_json::from_str::<BookmarkCursor>(number)
                    .unwrap_err()
                    .to_string(),
                "invalid Pixiv bookmark cursor"
            );
        }
    }
}
