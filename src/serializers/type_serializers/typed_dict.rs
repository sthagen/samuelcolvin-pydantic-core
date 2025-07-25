use pyo3::intern;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyString};

use ahash::AHashMap;

use crate::build_tools::py_schema_err;
use crate::build_tools::{py_schema_error_type, schema_or_config, ExtraBehavior};
use crate::definitions::DefinitionsBuilder;
use crate::tools::SchemaDict;

use super::{BuildSerializer, CombinedSerializer, ComputedFields, FieldsMode, GeneralFieldsSerializer, SerField};

#[derive(Debug)]
pub struct TypedDictBuilder;

impl BuildSerializer for TypedDictBuilder {
    const EXPECTED_TYPE: &'static str = "typed-dict";

    fn build(
        schema: &Bound<'_, PyDict>,
        config: Option<&Bound<'_, PyDict>>,
        definitions: &mut DefinitionsBuilder<CombinedSerializer>,
    ) -> PyResult<CombinedSerializer> {
        let py = schema.py();

        let total =
            schema_or_config(schema, config, intern!(py, "total"), intern!(py, "typed_dict_total"))?.unwrap_or(true);

        let fields_mode = match ExtraBehavior::from_schema_or_config(py, schema, config, ExtraBehavior::Ignore)? {
            ExtraBehavior::Allow => FieldsMode::TypedDictAllow,
            _ => FieldsMode::SimpleDict,
        };

        let serialize_by_alias = config.get_as(intern!(py, "serialize_by_alias"))?;

        let fields_dict: Bound<'_, PyDict> = schema.get_as_req(intern!(py, "fields"))?;
        let mut fields: AHashMap<String, SerField> = AHashMap::with_capacity(fields_dict.len());

        let extra_serializer = match (schema.get_item(intern!(py, "extras_schema"))?, &fields_mode) {
            (Some(v), FieldsMode::TypedDictAllow) => {
                Some(CombinedSerializer::build(&v.extract()?, config, definitions)?)
            }
            (Some(_), _) => return py_schema_err!("extras_schema can only be used if extra_behavior=allow"),
            (_, _) => None,
        };

        for (key, value) in fields_dict {
            let key_py = key.downcast_into::<PyString>()?;
            let key: String = key_py.extract()?;
            let field_info = value.downcast()?;

            let key_py: Py<PyString> = key_py.into();
            let required = field_info.get_as(intern!(py, "required"))?.unwrap_or(total);

            if field_info.get_as(intern!(py, "serialization_exclude"))? == Some(true) {
                fields.insert(
                    key,
                    SerField::new(py, key_py, None, None, required, serialize_by_alias, None),
                );
            } else {
                let alias: Option<String> = field_info.get_as(intern!(py, "serialization_alias"))?;
                let serialization_exclude_if: Option<Py<PyAny>> =
                    field_info.get_as(intern!(py, "serialization_exclude_if"))?;
                let schema = field_info.get_as_req(intern!(py, "schema"))?;
                let serializer = CombinedSerializer::build(&schema, config, definitions)
                    .map_err(|e| py_schema_error_type!("Field `{}`:\n  {}", key, e))?;
                fields.insert(
                    key,
                    SerField::new(
                        py,
                        key_py,
                        alias,
                        Some(serializer),
                        required,
                        serialize_by_alias,
                        serialization_exclude_if,
                    ),
                );
            }
        }

        let computed_fields = ComputedFields::new(schema, config, definitions)?;

        Ok(GeneralFieldsSerializer::new(fields, fields_mode, extra_serializer, computed_fields).into())
    }
}
