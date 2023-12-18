use h11;
use pyo3::prelude::*;

#[pyclass]
struct Client;

#[pyclass]
struct Server;

#[pyclass]
struct Connection(h11::Connection);

#[pymethods]
impl Connection {
    #[new]
    fn new(role: PyAny, max_incomplete_event_size: PyAny) -> Self {
        Connection(h11::Connection::new(h11::Role::Client, Some(0)))
    }
}

/// A Python module implemented in Rust. The name of this function must match
/// the `lib.name` setting in the `Cargo.toml`, else Python will not be able to
/// import the module.
#[pymodule]
fn rust_h11(_py: Python<'_>, m: &PyModule) -> PyResult<()> {
    Ok(())
}
