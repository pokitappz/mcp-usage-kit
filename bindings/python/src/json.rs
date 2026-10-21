//! Bounded conversion of JSON values, without calling arbitrary serialization hooks.

use std::collections::HashSet;

use pyo3::IntoPyObjectExt;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyFloat, PyInt, PyList, PyString, PyTuple};
use serde_json::{Map, Number, Value};

pub(crate) fn from_python(value: &Bound<'_, PyAny>) -> PyResult<Value> {
    convert(value, 0, &mut 100_000, &mut HashSet::new())
}

fn invalid() -> PyErr {
    PyValueError::new_err("response must contain bounded, acyclic JSON-compatible values")
}

fn convert(
    value: &Bound<'_, PyAny>,
    depth: usize,
    remaining: &mut usize,
    ancestors: &mut HashSet<usize>,
) -> PyResult<Value> {
    if depth >= 128 || *remaining == 0 {
        return Err(invalid());
    }
    *remaining -= 1;
    if value.is_none() {
        return Ok(Value::Null);
    }
    if value.is_instance_of::<PyBool>() {
        return value.extract().map(Value::Bool);
    }
    if value.is_instance_of::<PyInt>() {
        return value
            .extract::<i64>()
            .map(|v| Value::Number(v.into()))
            .or_else(|_| {
                value
                    .extract::<u64>()
                    .map(|v| Value::Number(v.into()))
                    .map_err(|_| invalid())
            });
    }
    if value.is_instance_of::<PyFloat>() {
        return Number::from_f64(value.extract()?)
            .map(Value::Number)
            .ok_or_else(invalid);
    }
    if let Ok(text) = value.cast::<PyString>() {
        return Ok(Value::String(text.to_str()?.to_owned()));
    }
    let address = value.as_ptr().addr();
    if !ancestors.insert(address) {
        return Err(invalid());
    }
    let result = if let Ok(dict) = value.cast::<PyDict>() {
        let mut object = Map::new();
        for (key, item) in dict {
            let key = key
                .cast::<PyString>()
                .map_err(|_| invalid())?
                .to_str()?
                .to_owned();
            object.insert(key, convert(&item, depth + 1, remaining, ancestors)?);
        }
        Ok(Value::Object(object))
    } else if value.is_instance_of::<PyList>() || value.is_instance_of::<PyTuple>() {
        let mut items = Vec::new();
        for item in value.try_iter()? {
            items.push(convert(&item?, depth + 1, remaining, ancestors)?);
        }
        Ok(Value::Array(items))
    } else {
        Err(invalid())
    };
    ancestors.remove(&address);
    result
}

pub(crate) fn to_python(py: Python<'_>, value: &Value) -> PyResult<Py<PyAny>> {
    match value {
        Value::Null => Ok(py.None()),
        Value::Bool(v) => v.into_py_any(py),
        Value::String(v) => v.into_py_any(py),
        Value::Number(v) => {
            if let Some(n) = v.as_i64() {
                n.into_py_any(py)
            } else if let Some(n) = v.as_u64() {
                n.into_py_any(py)
            } else {
                v.as_f64().ok_or_else(invalid)?.into_py_any(py)
            }
        }
        Value::Array(items) => {
            let values = items
                .iter()
                .map(|v| to_python(py, v))
                .collect::<PyResult<Vec<_>>>()?;
            Ok(PyList::new(py, values)?.into_any().unbind())
        }
        Value::Object(items) => {
            let dict = PyDict::new(py);
            for (key, value) in items {
                dict.set_item(key, to_python(py, value)?)?;
            }
            Ok(dict.into_any().unbind())
        }
    }
}
