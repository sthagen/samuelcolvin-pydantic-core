use std::borrow::Cow;
use std::fmt::Debug;
use std::io::{self, Write};

use pyo3::exceptions::PyTypeError;
use pyo3::prelude::*;
use pyo3::sync::GILOnceCell;
use pyo3::types::{PyDict, PyString};
use pyo3::{intern, PyTraverseError, PyVisit};

use enum_dispatch::enum_dispatch;
use serde::Serialize;
use serde_json::ser::{Formatter, PrettyFormatter};

use crate::build_tools::py_schema_err;
use crate::build_tools::py_schema_error_type;
use crate::definitions::DefinitionsBuilder;
use crate::py_gc::PyGcTraverse;
use crate::serializers::ser::PythonSerializer;
use crate::tools::{py_err, SchemaDict};

use super::errors::se_err_py_err;
use super::extra::Extra;
use super::infer::{infer_json_key, infer_serialize, infer_to_python};
use super::ob_type::{IsType, ObType};

pub(crate) trait BuildSerializer: Sized {
    const EXPECTED_TYPE: &'static str;

    fn build(
        schema: &Bound<'_, PyDict>,
        config: Option<&Bound<'_, PyDict>>,
        definitions: &mut DefinitionsBuilder<CombinedSerializer>,
    ) -> PyResult<CombinedSerializer>;
}

/// Build the `CombinedSerializer` enum and implement a `find_serializer` method for it.
macro_rules! combined_serializer {
    (
        enum_only: {$($e_key:ident: $e_serializer:path;)*}
        find_only: {$($builder:path;)*}
        both: {$($b_key:ident: $b_serializer:path;)*}
    ) => {
        #[derive(Debug)]
        #[enum_dispatch]
        pub enum CombinedSerializer {
            $($e_key($e_serializer),)*
            $($b_key($b_serializer),)*
        }

        impl CombinedSerializer {
            fn find_serializer(
                lookup_type: &str,
                schema: &Bound<'_, PyDict>,
                config: Option<&Bound<'_, PyDict>>,
                definitions: &mut DefinitionsBuilder<CombinedSerializer>
            ) -> PyResult<CombinedSerializer> {
                match lookup_type {
                    $(
                        <$b_serializer>::EXPECTED_TYPE => match <$b_serializer>::build(schema, config, definitions) {
                            Ok(serializer) => Ok(serializer),
                            Err(err) => py_schema_err!("Error building `{}` serializer:\n  {}", lookup_type, err),
                        },
                    )*
                    $(
                        <$builder>::EXPECTED_TYPE => match <$builder>::build(schema, config, definitions) {
                            Ok(serializer) => Ok(serializer),
                            Err(err) => py_schema_err!("Error building `{}` serializer:\n  {}", lookup_type, err),
                        },
                    )*
                    _ => py_schema_err!("Unknown serialization schema type: `{}`", lookup_type),
                }
            }
        }

    };
}

combined_serializer! {
    // `enum_only` is for type_serializers which are not built directly via the `type` key and `find_serializer`
    // but are included in the `CombinedSerializer` enum
    enum_only: {
        // function type_serializers cannot be defined by type lookup, but must be members of `CombinedSerializer`,
        // hence they're here.
        Function: super::type_serializers::function::FunctionPlainSerializer;
        FunctionWrap: super::type_serializers::function::FunctionWrapSerializer;
        Fields: super::fields::GeneralFieldsSerializer;
        // prebuilt serializers are manually constructed, and thus manually added to the `CombinedSerializer` enum
        Prebuilt: super::prebuilt::PrebuiltSerializer;
    }
    // `find_only` is for type_serializers which are built directly via the `type` key and `find_serializer`
    // but aren't actually used for serialization, e.g. their `build` method must return another serializer
    find_only: {
        super::type_serializers::other::ChainBuilder;
        super::type_serializers::other::CustomErrorBuilder;
        super::type_serializers::other::CallBuilder;
        super::type_serializers::other::LaxOrStrictBuilder;
        super::type_serializers::other::ArgumentsBuilder;
        super::type_serializers::other::IsInstanceBuilder;
        super::type_serializers::other::IsSubclassBuilder;
        super::type_serializers::other::CallableBuilder;
        super::type_serializers::definitions::DefinitionsSerializerBuilder;
        super::type_serializers::dataclass::DataclassArgsBuilder;
        super::type_serializers::function::FunctionBeforeSerializerBuilder;
        super::type_serializers::function::FunctionAfterSerializerBuilder;
        super::type_serializers::function::FunctionPlainSerializerBuilder;
        super::type_serializers::function::FunctionWrapSerializerBuilder;
        super::type_serializers::model::ModelFieldsBuilder;
        super::type_serializers::typed_dict::TypedDictBuilder;
    }
    // `both` means the struct is added to both the `CombinedSerializer` enum and the match statement in
    // `find_serializer` so they can be used via a `type` str.
    both: {
        None: super::type_serializers::simple::NoneSerializer;
        Nullable: super::type_serializers::nullable::NullableSerializer;
        Int: super::type_serializers::simple::IntSerializer;
        Bool: super::type_serializers::simple::BoolSerializer;
        Float: super::type_serializers::float::FloatSerializer;
        Decimal: super::type_serializers::decimal::DecimalSerializer;
        Str: super::type_serializers::string::StrSerializer;
        Bytes: super::type_serializers::bytes::BytesSerializer;
        Datetime: super::type_serializers::datetime_etc::DatetimeSerializer;
        TimeDelta: super::type_serializers::timedelta::TimeDeltaSerializer;
        Date: super::type_serializers::datetime_etc::DateSerializer;
        Time: super::type_serializers::datetime_etc::TimeSerializer;
        List: super::type_serializers::list::ListSerializer;
        Set: super::type_serializers::set_frozenset::SetSerializer;
        FrozenSet: super::type_serializers::set_frozenset::FrozenSetSerializer;
        Generator: super::type_serializers::generator::GeneratorSerializer;
        Dict: super::type_serializers::dict::DictSerializer;
        Model: super::type_serializers::model::ModelSerializer;
        Dataclass: super::type_serializers::dataclass::DataclassSerializer;
        Url: super::type_serializers::url::UrlSerializer;
        MultiHostUrl: super::type_serializers::url::MultiHostUrlSerializer;
        Uuid: super::type_serializers::uuid::UuidSerializer;
        Any: super::type_serializers::any::AnySerializer;
        Format: super::type_serializers::format::FormatSerializer;
        ToString: super::type_serializers::format::ToStringSerializer;
        WithDefault: super::type_serializers::with_default::WithDefaultSerializer;
        Json: super::type_serializers::json::JsonSerializer;
        JsonOrPython: super::type_serializers::json_or_python::JsonOrPythonSerializer;
        Union: super::type_serializers::union::UnionSerializer;
        TaggedUnion: super::type_serializers::union::TaggedUnionSerializer;
        Literal: super::type_serializers::literal::LiteralSerializer;
        MissingSentinel: super::type_serializers::missing_sentinel::MissingSentinelSerializer;
        Enum: super::type_serializers::enum_::EnumSerializer;
        Recursive: super::type_serializers::definitions::DefinitionRefSerializer;
        Tuple: super::type_serializers::tuple::TupleSerializer;
        Complex: super::type_serializers::complex::ComplexSerializer;
    }
}

impl CombinedSerializer {
    // Used when creating the base serializer instance, to avoid reusing the instance
    // when unpickling:
    pub fn build_base(
        schema: &Bound<'_, PyDict>,
        config: Option<&Bound<'_, PyDict>>,
        definitions: &mut DefinitionsBuilder<CombinedSerializer>,
    ) -> PyResult<CombinedSerializer> {
        Self::_build(schema, config, definitions, false)
    }

    fn _build(
        schema: &Bound<'_, PyDict>,
        config: Option<&Bound<'_, PyDict>>,
        definitions: &mut DefinitionsBuilder<CombinedSerializer>,
        use_prebuilt: bool,
    ) -> PyResult<CombinedSerializer> {
        let py = schema.py();
        let type_key = intern!(py, "type");

        if let Some(ser_schema) = schema.get_as::<Bound<'_, PyDict>>(intern!(py, "serialization"))? {
            let op_ser_type: Option<Bound<'_, PyString>> = ser_schema.get_as(type_key)?;
            match op_ser_type.as_ref().map(|py_str| py_str.to_str()).transpose()? {
                Some("function-plain") => {
                    // `function-plain` is a special case, not included in `find_serializer` since it means
                    // something different in `schema.type`
                    // NOTE! we use the `schema` here, not `ser_schema`
                    return super::type_serializers::function::FunctionPlainSerializer::build(
                        schema,
                        config,
                        definitions,
                    )
                    .map_err(|err| py_schema_error_type!("Error building `function-plain` serializer:\n  {}", err));
                }
                Some("function-wrap") => {
                    // `function-wrap` is also a special case, not included in `find_serializer` since it mean
                    // something different in `schema.type`
                    // NOTE! we use the `schema` here, not `ser_schema`
                    return super::type_serializers::function::FunctionWrapSerializer::build(
                        schema,
                        config,
                        definitions,
                    )
                    .map_err(|err| py_schema_error_type!("Error building `function-wrap` serializer:\n  {}", err));
                }
                // applies to lists tuples and dicts, does not override the main schema `type`
                Some("include-exclude-sequence" | "include-exclude-dict") => (),
                // applies specifically to bytes, does not override the main schema `type`
                Some("base64") => (),
                Some(ser_type) => {
                    // otherwise if `schema.serialization.type` is defined, use that with `find_serializer`
                    // instead of `schema.type`. In this case it's an error if a serializer isn't found.
                    return Self::find_serializer(ser_type, &ser_schema, config, definitions);
                }
                // if `schema.serialization.type` is None, fall back to `schema.type`
                None => (),
            };
        }

        let type_: Bound<'_, PyString> = schema.get_as_req(type_key)?;
        let type_ = type_.to_str()?;

        if use_prebuilt {
            // if we have a SchemaValidator on the type already, use it
            if let Ok(Some(prebuilt_serializer)) =
                super::prebuilt::PrebuiltSerializer::try_get_from_schema(type_, schema)
            {
                return Ok(prebuilt_serializer);
            }
        }

        Self::find_serializer(type_, schema, config, definitions)
    }

    /// Main recursive way to call serializers, supports possible recursive type inference by
    /// switching to type inference mode eagerly.
    pub fn to_python(
        &self,
        value: &Bound<'_, PyAny>,
        include: Option<&Bound<'_, PyAny>>,
        exclude: Option<&Bound<'_, PyAny>>,
        extra: &Extra,
    ) -> PyResult<PyObject> {
        if extra.serialize_as_any {
            infer_to_python(value, include, exclude, extra)
        } else {
            self.to_python_no_infer(value, include, exclude, extra)
        }
    }

    /// Variant of the above which does not fall back to inference mode immediately
    #[inline]
    pub fn to_python_no_infer(
        &self,
        value: &Bound<'_, PyAny>,
        include: Option<&Bound<'_, PyAny>>,
        exclude: Option<&Bound<'_, PyAny>>,
        extra: &Extra,
    ) -> PyResult<PyObject> {
        TypeSerializer::to_python(self, value, include, exclude, extra)
    }

    pub fn json_key<'a>(&self, key: &'a Bound<'_, PyAny>, extra: &Extra) -> PyResult<Cow<'a, str>> {
        if extra.serialize_as_any {
            infer_json_key(key, extra)
        } else {
            self.json_key_no_infer(key, extra)
        }
    }

    #[inline]
    pub fn json_key_no_infer<'a>(&self, key: &'a Bound<'_, PyAny>, extra: &Extra) -> PyResult<Cow<'a, str>> {
        TypeSerializer::json_key(self, key, extra)
    }

    pub fn serde_serialize<S: serde::ser::Serializer>(
        &self,
        value: &Bound<'_, PyAny>,
        serializer: S,
        include: Option<&Bound<'_, PyAny>>,
        exclude: Option<&Bound<'_, PyAny>>,
        extra: &Extra,
    ) -> Result<S::Ok, S::Error> {
        if extra.serialize_as_any {
            infer_serialize(value, serializer, include, exclude, extra)
        } else {
            self.serde_serialize_no_infer(value, serializer, include, exclude, extra)
        }
    }

    #[inline]
    pub fn serde_serialize_no_infer<S: serde::ser::Serializer>(
        &self,
        value: &Bound<'_, PyAny>,
        serializer: S,
        include: Option<&Bound<'_, PyAny>>,
        exclude: Option<&Bound<'_, PyAny>>,
        extra: &Extra,
    ) -> Result<S::Ok, S::Error> {
        TypeSerializer::serde_serialize(self, value, serializer, include, exclude, extra)
    }
}

impl BuildSerializer for CombinedSerializer {
    // this value is never used, it's just here to satisfy the trait
    const EXPECTED_TYPE: &'static str = "";

    fn build(
        schema: &Bound<'_, PyDict>,
        config: Option<&Bound<'_, PyDict>>,
        definitions: &mut DefinitionsBuilder<CombinedSerializer>,
    ) -> PyResult<CombinedSerializer> {
        Self::_build(schema, config, definitions, true)
    }
}

// Implemented by hand because `enum_dispatch` fails with a proc macro compile error =/
impl PyGcTraverse for CombinedSerializer {
    fn py_gc_traverse(&self, visit: &PyVisit<'_>) -> Result<(), PyTraverseError> {
        match self {
            CombinedSerializer::Function(inner) => inner.py_gc_traverse(visit),
            CombinedSerializer::FunctionWrap(inner) => inner.py_gc_traverse(visit),
            CombinedSerializer::Fields(inner) => inner.py_gc_traverse(visit),
            CombinedSerializer::Prebuilt(inner) => inner.py_gc_traverse(visit),
            CombinedSerializer::None(inner) => inner.py_gc_traverse(visit),
            CombinedSerializer::Nullable(inner) => inner.py_gc_traverse(visit),
            CombinedSerializer::Int(inner) => inner.py_gc_traverse(visit),
            CombinedSerializer::Bool(inner) => inner.py_gc_traverse(visit),
            CombinedSerializer::Float(inner) => inner.py_gc_traverse(visit),
            CombinedSerializer::Decimal(inner) => inner.py_gc_traverse(visit),
            CombinedSerializer::Str(inner) => inner.py_gc_traverse(visit),
            CombinedSerializer::Bytes(inner) => inner.py_gc_traverse(visit),
            CombinedSerializer::Datetime(inner) => inner.py_gc_traverse(visit),
            CombinedSerializer::TimeDelta(inner) => inner.py_gc_traverse(visit),
            CombinedSerializer::Date(inner) => inner.py_gc_traverse(visit),
            CombinedSerializer::Time(inner) => inner.py_gc_traverse(visit),
            CombinedSerializer::List(inner) => inner.py_gc_traverse(visit),
            CombinedSerializer::Set(inner) => inner.py_gc_traverse(visit),
            CombinedSerializer::FrozenSet(inner) => inner.py_gc_traverse(visit),
            CombinedSerializer::Generator(inner) => inner.py_gc_traverse(visit),
            CombinedSerializer::Dict(inner) => inner.py_gc_traverse(visit),
            CombinedSerializer::Model(inner) => inner.py_gc_traverse(visit),
            CombinedSerializer::Dataclass(inner) => inner.py_gc_traverse(visit),
            CombinedSerializer::Url(inner) => inner.py_gc_traverse(visit),
            CombinedSerializer::MultiHostUrl(inner) => inner.py_gc_traverse(visit),
            CombinedSerializer::Any(inner) => inner.py_gc_traverse(visit),
            CombinedSerializer::Format(inner) => inner.py_gc_traverse(visit),
            CombinedSerializer::ToString(inner) => inner.py_gc_traverse(visit),
            CombinedSerializer::WithDefault(inner) => inner.py_gc_traverse(visit),
            CombinedSerializer::Json(inner) => inner.py_gc_traverse(visit),
            CombinedSerializer::JsonOrPython(inner) => inner.py_gc_traverse(visit),
            CombinedSerializer::Union(inner) => inner.py_gc_traverse(visit),
            CombinedSerializer::TaggedUnion(inner) => inner.py_gc_traverse(visit),
            CombinedSerializer::Literal(inner) => inner.py_gc_traverse(visit),
            CombinedSerializer::MissingSentinel(inner) => inner.py_gc_traverse(visit),
            CombinedSerializer::Enum(inner) => inner.py_gc_traverse(visit),
            CombinedSerializer::Recursive(inner) => inner.py_gc_traverse(visit),
            CombinedSerializer::Tuple(inner) => inner.py_gc_traverse(visit),
            CombinedSerializer::Uuid(inner) => inner.py_gc_traverse(visit),
            CombinedSerializer::Complex(inner) => inner.py_gc_traverse(visit),
        }
    }
}

#[enum_dispatch(CombinedSerializer)]
pub(crate) trait TypeSerializer: Send + Sync + Debug {
    fn to_python(
        &self,
        value: &Bound<'_, PyAny>,
        include: Option<&Bound<'_, PyAny>>,
        exclude: Option<&Bound<'_, PyAny>>,
        extra: &Extra,
    ) -> PyResult<PyObject>;

    fn json_key<'a>(&self, key: &'a Bound<'_, PyAny>, extra: &Extra) -> PyResult<Cow<'a, str>>;

    fn invalid_as_json_key<'a>(
        &self,
        key: &'a Bound<'_, PyAny>,
        extra: &Extra,
        expected_type: &'static str,
    ) -> PyResult<Cow<'a, str>> {
        match extra.ob_type_lookup.is_type(key, ObType::None) {
            IsType::Exact | IsType::Subclass => py_err!(PyTypeError; "`{}` not valid as object key", expected_type),
            IsType::False => {
                extra.warnings.on_fallback_py(self.get_name(), key, extra)?;
                infer_json_key(key, extra)
            }
        }
    }

    fn serde_serialize<S: serde::ser::Serializer>(
        &self,
        value: &Bound<'_, PyAny>,
        serializer: S,
        include: Option<&Bound<'_, PyAny>>,
        exclude: Option<&Bound<'_, PyAny>>,
        extra: &Extra,
    ) -> Result<S::Ok, S::Error>;

    fn get_name(&self) -> &str;

    /// Used by union serializers to decide if it's worth trying again while allowing subclasses
    fn retry_with_lax_check(&self) -> bool {
        false
    }

    fn get_default(&self, _py: Python) -> PyResult<Option<PyObject>> {
        Ok(None)
    }
}

pub(crate) struct PydanticSerializer<'py> {
    value: &'py Bound<'py, PyAny>,
    serializer: &'py CombinedSerializer,
    include: Option<&'py Bound<'py, PyAny>>,
    exclude: Option<&'py Bound<'py, PyAny>>,
    extra: &'py Extra<'py>,
}

impl<'py> PydanticSerializer<'py> {
    pub(crate) fn new(
        value: &'py Bound<'py, PyAny>,
        serializer: &'py CombinedSerializer,
        include: Option<&'py Bound<'py, PyAny>>,
        exclude: Option<&'py Bound<'py, PyAny>>,
        extra: &'py Extra<'py>,
    ) -> Self {
        Self {
            value,
            serializer,
            include,
            exclude,
            extra,
        }
    }
}

impl Serialize for PydanticSerializer<'_> {
    fn serialize<S: serde::ser::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.serializer
            .serde_serialize(self.value, serializer, self.include, self.exclude, self.extra)
    }
}

struct EscapeNonAsciiFormatter;

impl Formatter for EscapeNonAsciiFormatter {
    fn write_string_fragment<W: ?Sized + Write>(&mut self, writer: &mut W, fragment: &str) -> io::Result<()> {
        let mut input = fragment;

        while let Some((idx, non_ascii_char)) = input.chars().enumerate().find(|(_, c)| !c.is_ascii()) {
            if idx > 0 {
                // write all ascii characters before the non-ascii one
                let ascii_run = &input[..idx];
                writer.write_all(ascii_run.as_bytes()).unwrap();
            }

            let codepoint = non_ascii_char as u32;
            if codepoint < 0xFFFF {
                // write basic codepoint as single escape
                write!(writer, "\\u{codepoint:04x}").unwrap();
            } else {
                // encode extended plane character as utf16 pair
                for escape in non_ascii_char.encode_utf16(&mut [0; 2]) {
                    write!(writer, "\\u{escape:04x}").unwrap();
                }
            }

            input = &input[(idx + non_ascii_char.len_utf8())..];
        }

        // write any ascii trailer
        writer.write_all(input.as_bytes())?;
        Ok(())
    }
}

struct EscapeNonAsciiPrettyFormatter<'a> {
    pretty: PrettyFormatter<'a>,
    escape_non_ascii: EscapeNonAsciiFormatter,
}

impl<'a> EscapeNonAsciiPrettyFormatter<'a> {
    pub fn with_indent(indent: &'a [u8]) -> Self {
        Self {
            pretty: PrettyFormatter::with_indent(indent),
            escape_non_ascii: EscapeNonAsciiFormatter,
        }
    }
}

macro_rules! defer {
    ($formatter:ident, $fun:ident) => {
        fn $fun<W>(&mut self, writer: &mut W) -> io::Result<()>
        where
            W: ?Sized + io::Write,
        {
            self.$formatter.$fun(writer)
        }
    };
    ($formatter:ident, $fun:ident, $val:ty) => {
        fn $fun<W>(&mut self, writer: &mut W, val: $val) -> io::Result<()>
        where
            W: ?Sized + io::Write,
        {
            self.$formatter.$fun(writer, val)
        }
    };
}

#[allow(clippy::needless_lifetimes)]
impl Formatter for EscapeNonAsciiPrettyFormatter<'_> {
    defer!(escape_non_ascii, write_string_fragment, &str);
    defer!(pretty, begin_array);
    defer!(pretty, end_array);
    defer!(pretty, begin_array_value, bool);
    defer!(pretty, end_array_value);
    defer!(pretty, begin_object);
    defer!(pretty, end_object);
    defer!(pretty, begin_object_key, bool);
    defer!(pretty, end_object_key);
    defer!(pretty, begin_object_value);
    defer!(pretty, end_object_value);
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn to_json_bytes(
    value: &Bound<'_, PyAny>,
    serializer: &CombinedSerializer,
    include: Option<&Bound<'_, PyAny>>,
    exclude: Option<&Bound<'_, PyAny>>,
    extra: &Extra,
    indent: Option<usize>,
    ensure_ascii: bool,
    expected_json_size: usize,
) -> PyResult<Vec<u8>> {
    let serializer = PydanticSerializer::new(value, serializer, include, exclude, extra);

    let writer: Vec<u8> = Vec::with_capacity(expected_json_size);

    let bytes = match (indent, ensure_ascii) {
        (Some(indent), true) => {
            let indent = vec![b' '; indent];
            let formatter = EscapeNonAsciiPrettyFormatter::with_indent(&indent);
            let mut ser = PythonSerializer::with_formatter(writer, formatter);
            serializer.serialize(&mut ser).map_err(se_err_py_err)?;
            ser.into_inner()
        }
        (Some(indent), false) => {
            let indent = vec![b' '; indent];
            let formatter = PrettyFormatter::with_indent(&indent);
            let mut ser = PythonSerializer::with_formatter(writer, formatter);
            serializer.serialize(&mut ser).map_err(se_err_py_err)?;
            ser.into_inner()
        }
        (None, true) => {
            let mut ser = PythonSerializer::with_formatter(writer, EscapeNonAsciiFormatter);
            serializer.serialize(&mut ser).map_err(se_err_py_err)?;
            ser.into_inner()
        }
        (None, false) => {
            let mut ser = PythonSerializer::new(writer);
            serializer.serialize(&mut ser).map_err(se_err_py_err)?;
            ser.into_inner()
        }
    };

    Ok(bytes)
}

#[allow(clippy::type_complexity)]
pub(super) fn any_dataclass_iter<'a, 'py>(
    dataclass: &'a Bound<'py, PyAny>,
) -> PyResult<(
    impl Iterator<Item = PyResult<(Bound<'py, PyAny>, Bound<'py, PyAny>)>> + 'a,
    Bound<'py, PyDict>,
)>
where
    'py: 'a,
{
    let py = dataclass.py();
    let fields = dataclass
        .getattr(intern!(py, "__dataclass_fields__"))?
        .downcast_into::<PyDict>()?;
    let field_type_marker = get_field_marker(py)?;

    let next = move |(field_name, field): (Bound<'py, PyAny>, Bound<'py, PyAny>)| -> PyResult<Option<(Bound<'py, PyAny>, Bound<'py, PyAny>)>> {
        let field_type = field.getattr(intern!(py, "_field_type"))?;
        if field_type.is(field_type_marker) {
            let value = dataclass.getattr(field_name.downcast::<PyString>()?)?;
            Ok(Some((field_name, value)))
        } else {
            Ok(None)
        }
    };

    Ok((fields.iter().filter_map(move |field| next(field).transpose()), fields))
}

static DC_FIELD_MARKER: GILOnceCell<PyObject> = GILOnceCell::new();

/// needed to match the logic from dataclasses.fields `tuple(f for f in fields.values() if f._field_type is _FIELD)`
fn get_field_marker(py: Python<'_>) -> PyResult<&Bound<'_, PyAny>> {
    DC_FIELD_MARKER.import(py, "dataclasses", "_FIELD")
}
