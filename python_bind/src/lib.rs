use h11;
use pyo3::prelude::*;

#[pyclass]
struct Connection(h11::Connection);

#[pyclass]
#[derive(Clone, Copy)]
enum Role {
    Client,
    Server,
}

impl Into<h11::Role> for Role {
    fn into(self) -> h11::Role {
        match self {
            Role::Client => h11::Role::Client,
            Role::Server => h11::Role::Server,
        }
    }
}

impl From<h11::Role> for Role {
    fn from(role: h11::Role) -> Self {
        match role {
            h11::Role::Client => Role::Client,
            h11::Role::Server => Role::Server,
        }
    }
}

#[pymethods]
impl Connection {
    #[new]
    fn new(role: &Role, max_incomplete_event_size: Option<usize>) -> Self {
        Connection(h11::Connection::new(
            (*role).into(),
            max_incomplete_event_size,
        ))
    }

    #[getter]
    fn our_role(&self) -> Role {
        self.0.our_role.into()
    }

    #[getter]
    fn their_role(&self) -> Role {
        self.0.their_role.into()
    }

    #[getter]
    fn their_http_version(&self) -> Option<Vec<u8>> {
        self.0.their_http_version.clone()
    }

    #[getter]
    fn client_is_waiting_for_100_continue(&self) -> bool {
        self.0.client_is_waiting_for_100_continue
    }
}

/// A Python module implemented in Rust. The name of this function must match
/// the `lib.name` setting in the `Cargo.toml`, else Python will not be able to
/// import the module.
#[pymodule]
fn _rust(_py: Python<'_>, m: &PyModule) -> PyResult<()> {
    m.add_class::<Connection>()?;
    m.add_class::<Role>()?;
    Ok(())
}
