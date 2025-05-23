use std::borrow::Cow;

use jiter::{JsonArray, JsonObject, JsonValue};
use num_traits::cast::ToPrimitive;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyString};
use speedate::MicrosecondsPrecisionOverflowBehavior;
use strum::EnumMessage;

use crate::errors::{ErrorType, ErrorTypeDefaults, InputValue, LocItem, ValError, ValResult};
use crate::input::return_enums::EitherComplex;
use crate::lookup_key::{LookupKey, LookupPath};
use crate::validators::complex::string_to_complex;
use crate::validators::decimal::create_decimal;
use crate::validators::ValBytesMode;

use super::datetime::{
    bytes_as_date, bytes_as_datetime, bytes_as_time, bytes_as_timedelta, float_as_datetime, float_as_duration,
    float_as_time, int_as_datetime, int_as_duration, int_as_time, EitherDate, EitherDateTime, EitherTime,
};
use super::input_abstract::{ConsumeIterator, Never, ValMatch};
use super::return_enums::ValidationMatch;
use super::shared::{float_as_int, int_as_bool, str_as_bool, str_as_float, str_as_int};
use super::{
    Arguments, BorrowInput, EitherBytes, EitherFloat, EitherInt, EitherString, EitherTimedelta, GenericIterator, Input,
    KeywordArgs, PositionalArgs, ValidatedDict, ValidatedList, ValidatedSet, ValidatedTuple,
};

/// This is required but since JSON object keys are always strings, I don't think it can be called
impl From<&JsonValue<'_>> for LocItem {
    fn from(json_value: &JsonValue) -> Self {
        match json_value {
            JsonValue::Int(i) => (*i).into(),
            JsonValue::Str(s) => s.clone().into(),
            v => format!("{v:?}").into(),
        }
    }
}

impl From<JsonValue<'_>> for LocItem {
    fn from(json_value: JsonValue) -> Self {
        (&json_value).into()
    }
}

impl<'py, 'data> Input<'py> for JsonValue<'data> {
    #[inline]
    fn py_converter(&self) -> impl IntoPyObject<'py> + '_ {
        self
    }

    fn as_error_value(&self) -> InputValue {
        // cloning JsonValue is cheap due to use of Arc
        InputValue::Json(self.to_static())
    }

    fn is_none(&self) -> bool {
        matches!(self, JsonValue::Null)
    }

    fn as_kwargs(&self, py: Python<'py>) -> Option<Bound<'py, PyDict>> {
        match self {
            JsonValue::Object(object) => {
                let dict = PyDict::new(py);
                for (k, v) in object.as_slice() {
                    // TODO: jiter doesn't deduplicate keys, so we should probably do that here to
                    // avoid potential wasted work creating Python objects.
                    dict.set_item(k, v).unwrap();
                }
                Some(dict)
            }
            _ => None,
        }
    }

    type Arguments<'a>
        = JsonArgs<'a, 'data>
    where
        Self: 'a;

    fn validate_args(&self) -> ValResult<JsonArgs<'_, 'data>> {
        match self {
            JsonValue::Object(object) => Ok(JsonArgs::new(None, Some(object))),
            JsonValue::Array(array) => Ok(JsonArgs::new(Some(array), None)),
            _ => Err(ValError::new(ErrorTypeDefaults::ArgumentsType, self)),
        }
    }

    #[cfg_attr(has_coverage_attribute, coverage(off))]
    fn validate_args_v3(&self) -> ValResult<Self::Arguments<'_>> {
        Err(ValError::new(ErrorTypeDefaults::ArgumentsType, self))
    }

    fn validate_dataclass_args<'a>(&'a self, class_name: &str) -> ValResult<JsonArgs<'a, 'data>> {
        match self {
            JsonValue::Object(object) => Ok(JsonArgs::new(None, Some(object))),
            _ => {
                let class_name = class_name.to_string();
                Err(ValError::new(
                    ErrorType::DataclassType {
                        class_name,
                        context: None,
                    },
                    self,
                ))
            }
        }
    }

    fn validate_str(&self, strict: bool, coerce_numbers_to_str: bool) -> ValResult<ValidationMatch<EitherString<'_>>> {
        // Justification for `strict` instead of `exact` is that in JSON strings can also
        // represent other datatypes such as UUID and date more exactly, so string is a
        // converting input
        // TODO: in V3 we may want to make JSON str always win if in union, for consistency,
        // see https://github.com/pydantic/pydantic-core/pull/867#discussion_r1386582501
        match self {
            JsonValue::Str(s) => Ok(ValidationMatch::strict(s.as_ref().into())),
            JsonValue::Int(i) if !strict && coerce_numbers_to_str => Ok(ValidationMatch::lax(i.to_string().into())),
            JsonValue::BigInt(b) if !strict && coerce_numbers_to_str => Ok(ValidationMatch::lax(b.to_string().into())),
            JsonValue::Float(f) if !strict && coerce_numbers_to_str => Ok(ValidationMatch::lax(f.to_string().into())),
            _ => Err(ValError::new(ErrorTypeDefaults::StringType, self)),
        }
    }

    fn validate_bytes<'a>(
        &'a self,
        _strict: bool,
        mode: ValBytesMode,
    ) -> ValResult<ValidationMatch<EitherBytes<'a, 'py>>> {
        match self {
            JsonValue::Str(s) => match mode.deserialize_string(s) {
                Ok(b) => Ok(ValidationMatch::strict(b)),
                Err(e) => Err(ValError::new(e, self)),
            },
            _ => Err(ValError::new(ErrorTypeDefaults::BytesType, self)),
        }
    }

    fn validate_bool(&self, strict: bool) -> ValResult<ValidationMatch<bool>> {
        match self {
            JsonValue::Bool(b) => Ok(ValidationMatch::exact(*b)),
            JsonValue::Str(s) if !strict => str_as_bool(self, s).map(ValidationMatch::lax),
            JsonValue::Int(int) if !strict => int_as_bool(self, *int).map(ValidationMatch::lax),
            JsonValue::Float(float) if !strict => match float_as_int(self, *float) {
                Ok(int) => int
                    .as_bool()
                    .ok_or_else(|| ValError::new(ErrorTypeDefaults::BoolParsing, self))
                    .map(ValidationMatch::lax),
                _ => Err(ValError::new(ErrorTypeDefaults::BoolType, self)),
            },
            _ => Err(ValError::new(ErrorTypeDefaults::BoolType, self)),
        }
    }

    fn validate_int(&self, strict: bool) -> ValResult<ValidationMatch<EitherInt<'_>>> {
        match self {
            JsonValue::Int(i) => Ok(ValidationMatch::exact(EitherInt::I64(*i))),
            JsonValue::BigInt(b) => Ok(ValidationMatch::exact(EitherInt::BigInt(b.clone()))),
            JsonValue::Bool(b) if !strict => Ok(ValidationMatch::lax(EitherInt::I64((*b).into()))),
            JsonValue::Float(f) if !strict => float_as_int(self, *f).map(ValidationMatch::lax),
            JsonValue::Str(str) if !strict => str_as_int(self, str).map(ValidationMatch::lax),
            _ => Err(ValError::new(ErrorTypeDefaults::IntType, self)),
        }
    }

    fn exact_str(&self) -> ValResult<EitherString<'_>> {
        match self {
            JsonValue::Str(s) => Ok(s.as_ref().into()),
            _ => Err(ValError::new(ErrorTypeDefaults::StringType, self)),
        }
    }

    fn validate_float(&self, strict: bool) -> ValResult<ValidationMatch<EitherFloat<'_>>> {
        match self {
            JsonValue::Float(f) => Ok(ValidationMatch::exact(EitherFloat::F64(*f))),
            JsonValue::Int(i) => Ok(ValidationMatch::strict(EitherFloat::F64(*i as f64))),
            JsonValue::BigInt(b) => Ok(ValidationMatch::strict(EitherFloat::F64(
                b.to_f64().expect("BigInt should always return some value"),
            ))),
            JsonValue::Bool(b) if !strict => Ok(ValidationMatch::lax(EitherFloat::F64(if *b { 1.0 } else { 0.0 }))),
            JsonValue::Str(str) if !strict => str_as_float(self, str).map(ValidationMatch::lax),
            _ => Err(ValError::new(ErrorTypeDefaults::FloatType, self)),
        }
    }

    fn validate_decimal(&self, _strict: bool, py: Python<'py>) -> ValMatch<Bound<'py, PyAny>> {
        match self {
            JsonValue::Float(f) => {
                create_decimal(&PyString::new(py, &f.to_string()), self).map(ValidationMatch::strict)
            }
            JsonValue::Str(..) | JsonValue::Int(..) | JsonValue::BigInt(..) => {
                create_decimal(&self.into_pyobject(py)?, self).map(ValidationMatch::strict)
            }
            _ => Err(ValError::new(ErrorTypeDefaults::DecimalType, self)),
        }
    }

    type Dict<'a>
        = &'a JsonObject<'data>
    where
        Self: 'a;

    fn validate_dict(&self, _strict: bool) -> ValResult<Self::Dict<'_>> {
        match self {
            JsonValue::Object(dict) => Ok(dict),
            _ => Err(ValError::new(ErrorTypeDefaults::DictType, self)),
        }
    }
    #[cfg_attr(has_coverage_attribute, coverage(off))]
    fn strict_dict(&self) -> ValResult<Self::Dict<'_>> {
        self.validate_dict(false)
    }

    type List<'a>
        = &'a JsonArray<'data>
    where
        Self: 'a;

    fn validate_list(&self, _strict: bool) -> ValMatch<&JsonArray<'data>> {
        match self {
            JsonValue::Array(a) => Ok(ValidationMatch::exact(a)),
            _ => Err(ValError::new(ErrorTypeDefaults::ListType, self)),
        }
    }

    type Tuple<'a>
        = &'a JsonArray<'data>
    where
        Self: 'a;

    fn validate_tuple(&self, _strict: bool) -> ValMatch<&JsonArray<'data>> {
        // just as in set's case, List has to be allowed
        match self {
            JsonValue::Array(a) => Ok(ValidationMatch::strict(a)),
            _ => Err(ValError::new(ErrorTypeDefaults::TupleType, self)),
        }
    }

    type Set<'a>
        = &'a JsonArray<'data>
    where
        Self: 'a;

    fn validate_set(&self, _strict: bool) -> ValMatch<&JsonArray<'data>> {
        // we allow a list here since otherwise it would be impossible to create a set from JSON
        match self {
            JsonValue::Array(a) => Ok(ValidationMatch::strict(a)),
            _ => Err(ValError::new(ErrorTypeDefaults::SetType, self)),
        }
    }

    fn validate_frozenset(&self, _strict: bool) -> ValMatch<&JsonArray<'data>> {
        // we allow a list here since otherwise it would be impossible to create a frozenset from JSON
        match self {
            JsonValue::Array(a) => Ok(ValidationMatch::strict(a)),
            _ => Err(ValError::new(ErrorTypeDefaults::FrozenSetType, self)),
        }
    }

    fn validate_iter(&self) -> ValResult<GenericIterator<'static>> {
        match self {
            JsonValue::Array(a) => Ok(GenericIterator::from(a.clone()).into_static()),
            JsonValue::Str(s) => Ok(string_to_vec(s).into()),
            JsonValue::Object(object) => {
                // return keys iterator to match python's behavior
                // FIXME jiter doesn't deduplicate keys, should probably do that here before iteration.
                let keys: JsonArray = JsonArray::new(
                    object
                        .as_slice()
                        .iter()
                        .map(|(k, _)| JsonValue::Str(k.clone()))
                        .collect(),
                );
                Ok(GenericIterator::from(keys).into_static())
            }
            _ => Err(ValError::new(ErrorTypeDefaults::IterableType, self)),
        }
    }

    fn validate_date(&self, _strict: bool) -> ValResult<ValidationMatch<EitherDate<'py>>> {
        match self {
            JsonValue::Str(v) => bytes_as_date(self, v.as_bytes()).map(ValidationMatch::strict),
            _ => Err(ValError::new(ErrorTypeDefaults::DateType, self)),
        }
    }
    fn validate_time(
        &self,
        strict: bool,
        microseconds_overflow_behavior: MicrosecondsPrecisionOverflowBehavior,
    ) -> ValResult<ValidationMatch<EitherTime<'py>>> {
        match self {
            JsonValue::Str(v) => {
                bytes_as_time(self, v.as_bytes(), microseconds_overflow_behavior).map(ValidationMatch::strict)
            }
            JsonValue::Int(v) if !strict => int_as_time(self, *v, 0).map(ValidationMatch::lax),
            JsonValue::Float(v) if !strict => float_as_time(self, *v).map(ValidationMatch::lax),
            JsonValue::BigInt(_) if !strict => Err(ValError::new(
                ErrorType::TimeParsing {
                    error: Cow::Borrowed(
                        speedate::ParseError::TimeTooLarge
                            .get_documentation()
                            .unwrap_or_default(),
                    ),
                    context: None,
                },
                self,
            )),
            _ => Err(ValError::new(ErrorTypeDefaults::TimeType, self)),
        }
    }

    fn validate_datetime(
        &self,
        strict: bool,
        microseconds_overflow_behavior: speedate::MicrosecondsPrecisionOverflowBehavior,
    ) -> ValResult<ValidationMatch<EitherDateTime<'py>>> {
        match self {
            JsonValue::Str(v) => {
                bytes_as_datetime(self, v.as_bytes(), microseconds_overflow_behavior).map(ValidationMatch::strict)
            }
            JsonValue::Int(v) if !strict => int_as_datetime(self, *v, 0).map(ValidationMatch::lax),
            JsonValue::Float(v) if !strict => float_as_datetime(self, *v).map(ValidationMatch::lax),
            _ => Err(ValError::new(ErrorTypeDefaults::DatetimeType, self)),
        }
    }

    fn validate_timedelta(
        &self,
        strict: bool,
        microseconds_overflow_behavior: speedate::MicrosecondsPrecisionOverflowBehavior,
    ) -> ValResult<ValidationMatch<EitherTimedelta<'py>>> {
        match self {
            JsonValue::Str(v) => {
                bytes_as_timedelta(self, v.as_bytes(), microseconds_overflow_behavior).map(ValidationMatch::strict)
            }
            JsonValue::Int(v) if !strict => {
                int_as_duration(self, *v).map(|duration| ValidationMatch::lax(duration.into()))
            }
            JsonValue::Float(v) if !strict => {
                float_as_duration(self, *v).map(|duration| ValidationMatch::lax(duration.into()))
            }
            _ => Err(ValError::new(ErrorTypeDefaults::TimeDeltaType, self)),
        }
    }

    fn validate_complex(&self, strict: bool, py: Python<'py>) -> ValResult<ValidationMatch<EitherComplex<'py>>> {
        match self {
            JsonValue::Str(s) => Ok(ValidationMatch::strict(EitherComplex::Py(string_to_complex(
                &PyString::new(py, s),
                self,
            )?))),
            JsonValue::Float(f) => {
                if !strict {
                    Ok(ValidationMatch::lax(EitherComplex::Complex([*f, 0.0])))
                } else {
                    Err(ValError::new(ErrorTypeDefaults::ComplexStrParsing, self))
                }
            }
            JsonValue::Int(f) => {
                if !strict {
                    Ok(ValidationMatch::lax(EitherComplex::Complex([(*f) as f64, 0.0])))
                } else {
                    Err(ValError::new(ErrorTypeDefaults::ComplexStrParsing, self))
                }
            }
            _ => Err(ValError::new(ErrorTypeDefaults::ComplexType, self)),
        }
    }
}

/// Required for JSON Object keys so the string can behave like an Input
impl<'py> Input<'py> for str {
    #[inline]
    fn py_converter(&self) -> impl IntoPyObject<'py> + '_ {
        self
    }

    fn as_error_value(&self) -> InputValue {
        // Justification for the clone: this is on the error pathway and we are generally ok
        // with errors having a performance penalty
        InputValue::Json(JsonValue::Str(self.to_owned().into()))
    }

    fn as_kwargs(&self, _py: Python<'py>) -> Option<Bound<'py, PyDict>> {
        None
    }

    type Arguments<'a> = Never;

    #[cfg_attr(has_coverage_attribute, coverage(off))]
    fn validate_args(&self) -> ValResult<Never> {
        Err(ValError::new(ErrorTypeDefaults::ArgumentsType, self))
    }

    #[cfg_attr(has_coverage_attribute, coverage(off))]
    fn validate_args_v3(&self) -> ValResult<Never> {
        Err(ValError::new(ErrorTypeDefaults::ArgumentsType, self))
    }

    #[cfg_attr(has_coverage_attribute, coverage(off))]
    fn validate_dataclass_args(&self, class_name: &str) -> ValResult<Never> {
        let class_name = class_name.to_string();
        Err(ValError::new(
            ErrorType::DataclassType {
                class_name,
                context: None,
            },
            self,
        ))
    }

    fn validate_str(
        &self,
        _strict: bool,
        _coerce_numbers_to_str: bool,
    ) -> ValResult<ValidationMatch<EitherString<'_>>> {
        // Justification for `strict` instead of `exact` is that in JSON strings can also
        // represent other datatypes such as UUID and date more exactly, so string is a
        // converting input
        // TODO: in V3 we may want to make JSON str always win if in union, for consistency,
        // see https://github.com/pydantic/pydantic-core/pull/867#discussion_r1386582501
        Ok(ValidationMatch::strict(self.into()))
    }

    fn validate_bytes<'a>(
        &'a self,
        _strict: bool,
        mode: ValBytesMode,
    ) -> ValResult<ValidationMatch<EitherBytes<'a, 'py>>> {
        match mode.deserialize_string(self) {
            Ok(b) => Ok(ValidationMatch::strict(b)),
            Err(e) => Err(ValError::new(e, self)),
        }
    }

    fn validate_bool(&self, _strict: bool) -> ValResult<ValidationMatch<bool>> {
        str_as_bool(self, self).map(ValidationMatch::lax)
    }

    fn validate_int(&self, _strict: bool) -> ValResult<ValidationMatch<EitherInt<'_>>> {
        str_as_int(self, self).map(ValidationMatch::lax)
    }

    fn validate_float(&self, _strict: bool) -> ValResult<ValidationMatch<EitherFloat<'_>>> {
        str_as_float(self, self).map(ValidationMatch::lax)
    }

    fn validate_decimal(&self, _strict: bool, py: Python<'py>) -> ValMatch<Bound<'py, PyAny>> {
        create_decimal(self.into_pyobject(py)?.as_any(), self).map(ValidationMatch::lax)
    }

    type Dict<'a> = Never;

    #[cfg_attr(has_coverage_attribute, coverage(off))]
    fn strict_dict(&self) -> ValResult<Never> {
        Err(ValError::new(ErrorTypeDefaults::DictType, self))
    }

    type List<'a> = Never;

    fn validate_list(&self, _strict: bool) -> ValMatch<Never> {
        Err(ValError::new(ErrorTypeDefaults::ListType, self))
    }

    type Tuple<'a> = Never;

    #[cfg_attr(has_coverage_attribute, coverage(off))]
    fn validate_tuple(&self, _strict: bool) -> ValMatch<Never> {
        Err(ValError::new(ErrorTypeDefaults::TupleType, self))
    }

    type Set<'a> = Never;

    #[cfg_attr(has_coverage_attribute, coverage(off))]
    fn validate_set(&self, _strict: bool) -> ValMatch<Never> {
        Err(ValError::new(ErrorTypeDefaults::SetType, self))
    }

    #[cfg_attr(has_coverage_attribute, coverage(off))]
    fn validate_frozenset(&self, _strict: bool) -> ValMatch<Never> {
        Err(ValError::new(ErrorTypeDefaults::SetType, self))
    }

    fn validate_iter(&self) -> ValResult<GenericIterator<'static>> {
        Ok(string_to_vec(self).into())
    }

    fn validate_date(&self, _strict: bool) -> ValResult<ValidationMatch<EitherDate<'py>>> {
        bytes_as_date(self, self.as_bytes()).map(ValidationMatch::lax)
    }

    fn validate_time(
        &self,
        _strict: bool,
        microseconds_overflow_behavior: MicrosecondsPrecisionOverflowBehavior,
    ) -> ValResult<ValidationMatch<EitherTime<'py>>> {
        bytes_as_time(self, self.as_bytes(), microseconds_overflow_behavior).map(ValidationMatch::lax)
    }

    fn validate_datetime(
        &self,
        _strict: bool,
        microseconds_overflow_behavior: MicrosecondsPrecisionOverflowBehavior,
    ) -> ValResult<ValidationMatch<EitherDateTime<'py>>> {
        bytes_as_datetime(self, self.as_bytes(), microseconds_overflow_behavior).map(ValidationMatch::lax)
    }

    fn validate_timedelta(
        &self,
        _strict: bool,
        microseconds_overflow_behavior: MicrosecondsPrecisionOverflowBehavior,
    ) -> ValResult<ValidationMatch<EitherTimedelta<'py>>> {
        bytes_as_timedelta(self, self.as_bytes(), microseconds_overflow_behavior).map(ValidationMatch::lax)
    }

    fn validate_complex(&self, _strict: bool, py: Python<'py>) -> ValResult<ValidationMatch<EitherComplex<'py>>> {
        Ok(ValidationMatch::strict(EitherComplex::Py(string_to_complex(
            &self.into_pyobject(py)?,
            self,
        )?)))
    }
}

impl BorrowInput<'_> for &'_ String {
    type Input = str;
    fn borrow_input(&self) -> &Self::Input {
        self
    }
}

impl BorrowInput<'_> for String {
    type Input = str;
    fn borrow_input(&self) -> &Self::Input {
        self
    }
}

impl<'data> BorrowInput<'_> for JsonValue<'data> {
    type Input = JsonValue<'data>;
    fn borrow_input(&self) -> &Self::Input {
        self
    }
}

fn string_to_vec(s: &str) -> JsonArray<'static> {
    JsonArray::new(s.chars().map(|c| JsonValue::Str(c.to_string().into())).collect())
}

impl<'data> ValidatedDict<'_> for &'_ JsonObject<'data> {
    type Key<'a>
        = &'a str
    where
        Self: 'a;

    type Item<'a>
        = &'a JsonValue<'data>
    where
        Self: 'a;

    fn get_item<'k>(&self, key: &'k LookupKey) -> ValResult<Option<(&'k LookupPath, Self::Item<'_>)>> {
        key.json_get(self)
    }

    fn iterate<'a, R>(
        &'a self,
        consumer: impl ConsumeIterator<ValResult<(Self::Key<'a>, Self::Item<'a>)>, Output = R>,
    ) -> ValResult<R> {
        Ok(consumer.consume_iterator(self.as_slice().iter().map(|(k, v)| Ok((k.as_ref(), v)))))
    }

    fn last_key(&self) -> Option<Self::Key<'_>> {
        self.last().map(|(k, _)| k.as_ref())
    }
}

impl<'a, 'py, 'data> ValidatedList<'py> for &'a JsonArray<'data> {
    type Item = &'a JsonValue<'data>;

    fn len(&self) -> Option<usize> {
        Some(Vec::len(self))
    }
    fn iterate<R>(self, consumer: impl ConsumeIterator<PyResult<Self::Item>, Output = R>) -> ValResult<R> {
        Ok(consumer.consume_iterator(self.iter().map(Ok)))
    }
    fn as_py_list(&self) -> Option<&Bound<'py, PyList>> {
        None
    }
}

impl<'a, 'data> ValidatedTuple<'_> for &'a JsonArray<'data> {
    type Item = &'a JsonValue<'data>;

    fn len(&self) -> Option<usize> {
        Some(Vec::len(self))
    }
    fn try_for_each(self, mut f: impl FnMut(PyResult<Self::Item>) -> ValResult<()>) -> ValResult<()> {
        for item in self.iter() {
            f(Ok(item))?;
        }
        Ok(())
    }
    fn iterate<R>(self, consumer: impl ConsumeIterator<PyResult<Self::Item>, Output = R>) -> ValResult<R> {
        Ok(consumer.consume_iterator(self.iter().map(Ok)))
    }
}

impl<'a, 'data> ValidatedSet<'_> for &'a JsonArray<'data> {
    type Item = &'a JsonValue<'data>;

    fn iterate<R>(self, consumer: impl ConsumeIterator<PyResult<Self::Item>, Output = R>) -> ValResult<R> {
        Ok(consumer.consume_iterator(self.iter().map(Ok)))
    }
}

#[cfg_attr(debug_assertions, derive(Debug))]
pub struct JsonArgs<'a, 'data> {
    args: Option<&'a [JsonValue<'data>]>,
    kwargs: Option<&'a JsonObject<'data>>,
}

impl<'a, 'data> JsonArgs<'a, 'data> {
    fn new(args: Option<&'a [JsonValue<'data>]>, kwargs: Option<&'a JsonObject<'data>>) -> Self {
        Self { args, kwargs }
    }
}

impl<'data> Arguments<'_> for JsonArgs<'_, 'data> {
    type Args = [JsonValue<'data>];
    type Kwargs = JsonObject<'data>;

    fn args(&self) -> Option<&Self::Args> {
        self.args
    }

    fn kwargs(&self) -> Option<&Self::Kwargs> {
        self.kwargs
    }
}

impl<'data> PositionalArgs<'_> for [JsonValue<'data>] {
    type Item<'a>
        = &'a JsonValue<'data>
    where
        Self: 'a;

    fn len(&self) -> usize {
        <[JsonValue]>::len(self)
    }
    fn get_item(&self, index: usize) -> Option<Self::Item<'_>> {
        self.get(index)
    }
    fn iter(&self) -> impl Iterator<Item = Self::Item<'_>> {
        <[JsonValue]>::iter(self)
    }
}

impl<'data> KeywordArgs<'_> for JsonObject<'data> {
    type Key<'a>
        = &'a str
    where
        Self: 'a;
    type Item<'a>
        = &'a JsonValue<'data>
    where
        Self: 'a;

    fn len(&self) -> usize {
        Vec::len(self)
    }
    fn get_item<'k>(&self, key: &'k LookupKey) -> ValResult<Option<(&'k LookupPath, Self::Item<'_>)>> {
        key.json_get(self)
    }
    fn iter(&self) -> impl Iterator<Item = ValResult<(Self::Key<'_>, Self::Item<'_>)>> {
        self.as_slice().iter().map(|(k, v)| Ok((k.as_ref(), v)))
    }
}
