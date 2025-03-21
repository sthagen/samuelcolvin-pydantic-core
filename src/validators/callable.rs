use pyo3::prelude::*;
use pyo3::types::PyDict;

use crate::errors::{ErrorTypeDefaults, ValError, ValResult};
use crate::input::Input;

use super::validation_state::Exactness;
use super::{BuildValidator, CombinedValidator, DefinitionsBuilder, ValidationState, Validator};

#[derive(Debug, Clone)]
pub struct CallableValidator;

impl BuildValidator for CallableValidator {
    const EXPECTED_TYPE: &'static str = "callable";

    fn build(
        _schema: &Bound<'_, PyDict>,
        _config: Option<&Bound<'_, PyDict>>,
        _definitions: &mut DefinitionsBuilder<CombinedValidator>,
    ) -> PyResult<CombinedValidator> {
        Ok(Self.into())
    }
}

impl_py_gc_traverse!(CallableValidator {});

impl Validator for CallableValidator {
    fn validate<'py>(
        &self,
        _py: Python<'py>,
        input: &(impl Input<'py> + ?Sized),
        state: &mut ValidationState<'_, 'py>,
    ) -> ValResult<PyObject> {
        state.floor_exactness(Exactness::Lax);
        if let Some(py_input) = input.as_python() {
            if py_input.is_callable() {
                return Ok(py_input.clone().unbind());
            }
        }
        Err(ValError::new(ErrorTypeDefaults::CallableType, input))
    }

    fn get_name(&self) -> &str {
        Self::EXPECTED_TYPE
    }
}
