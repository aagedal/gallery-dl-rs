//! Ordered extractor writes against the live postprocessor keyword dictionary.
use crate::{Result, error, event::KeywordUpdate};
use serde_json::{Map, Number, Value};

pub(crate) fn apply(context: &mut Map<String, Value>, updates: &[KeywordUpdate]) -> Result<()> {
    for update in updates {
        match update {
            KeywordUpdate::Set { name, value } => {
                context.insert(name.clone(), value.clone());
            }
            KeywordUpdate::Remove(name) => {
                context.shift_remove(name);
            }
            KeywordUpdate::RemoveRequired(name) => {
                context
                    .shift_remove(name)
                    .ok_or_else(|| error("missing producer keyword for deletion"))?;
            }
            KeywordUpdate::OnResume(_) => {}
            KeywordUpdate::Increment(name) => {
                let value = match context.get(name) {
                    Some(Value::Bool(value)) => Value::from(i64::from(*value) + 1),
                    Some(Value::Number(value)) if value.is_f64() => {
                        let sum = value.as_f64().ok_or_else(invalid)? + 1.0;
                        Value::Number(Number::from_f64(sum).ok_or_else(invalid)?)
                    }
                    Some(Value::Number(value)) => {
                        let sum = value
                            .as_i128()
                            .and_then(|n| n.checked_add(1))
                            .ok_or_else(invalid)?;
                        Value::Number(Number::from_i128(sum).ok_or_else(invalid)?)
                    }
                    _ => return Err(invalid()),
                };
                context.insert(name.clone(), value);
            }
            KeywordUpdate::Format { name, template } => {
                let value = super::template::Template::parse(template)?.render(context)?;
                context.insert(name.clone(), value.into());
            }
        }
    }
    Ok(())
}
fn invalid() -> crate::Error {
    error("invalid producer counter or supported numeric limit exceeded")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn increments_preserve_source_numeric_types_and_reject_invalid_values() {
        for (input, expected) in [
            (json!(true), json!(2)),
            (json!(-2), json!(-1)),
            (json!(1.5), json!(2.5)),
            (json!(u64::MAX - 1), json!(u64::MAX)),
            (json!(u64::MAX), json!(u128::from(u64::MAX) + 1)),
        ] {
            let mut context = json!({"num":input}).as_object().unwrap().clone();
            apply(&mut context, &[KeywordUpdate::Increment("num".into())]).unwrap();
            assert_eq!(context["num"], expected);
        }
        for input in [
            json!(null),
            json!("2"),
            json!([]),
            json!({}),
            json!(i128::MAX),
        ] {
            let mut context = json!({"num":input}).as_object().unwrap().clone();
            assert!(apply(&mut context, &[KeywordUpdate::Increment("num".into())]).is_err());
        }
        assert!(apply(&mut Map::new(), &[KeywordUpdate::Increment("num".into())]).is_err());
    }
    #[test]
    fn producer_deletion_retains_dictionary_insertion_order() {
        let mut context = json!({"first":1,"removed":2,"third":3,"last":4})
            .as_object()
            .unwrap()
            .clone();
        apply(
            &mut context,
            &[
                KeywordUpdate::RemoveRequired("removed".into()),
                KeywordUpdate::Set {
                    name: "added".into(),
                    value: json!(5),
                },
            ],
        )
        .unwrap();
        assert_eq!(
            context.keys().map(String::as_str).collect::<Vec<_>>(),
            ["first", "third", "last", "added"]
        );
        apply(&mut context, &[KeywordUpdate::Remove("missing".into())]).unwrap();
        assert!(
            apply(
                &mut context,
                &[KeywordUpdate::RemoveRequired("missing".into())]
            )
            .is_err()
        );
    }
}
