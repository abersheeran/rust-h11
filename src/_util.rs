#[derive(Debug)]
pub struct LocalProtocolError {
    pub message: String,
    pub code: u16,
}

impl From<(String, u16)> for LocalProtocolError {
    fn from(value: (String, u16)) -> Self {
        LocalProtocolError {
            message: value.0,
            code: value.1,
        }
    }
}

impl From<(&str, u16)> for LocalProtocolError {
    fn from(value: (&str, u16)) -> Self {
        LocalProtocolError {
            message: value.0.to_string(),
            code: value.1,
        }
    }
}

impl From<String> for LocalProtocolError {
    fn from(value: String) -> Self {
        LocalProtocolError {
            message: value,
            code: 400,
        }
    }
}

impl From<&str> for LocalProtocolError {
    fn from(value: &str) -> Self {
        LocalProtocolError {
            message: value.to_string(),
            code: 400,
        }
    }
}

impl LocalProtocolError {
    pub(crate) fn _reraise_as_remote_protocol_error(self) -> RemoteProtocolError {
        RemoteProtocolError {
            message: self.message,
            code: self.code,
        }
    }
}

#[derive(Debug)]
pub struct RemoteProtocolError {
    pub message: String,
    pub code: u16,
}

impl From<(String, u16)> for RemoteProtocolError {
    fn from(value: (String, u16)) -> Self {
        RemoteProtocolError {
            message: value.0,
            code: value.1,
        }
    }
}

impl From<(&str, u16)> for RemoteProtocolError {
    fn from(value: (&str, u16)) -> Self {
        RemoteProtocolError {
            message: value.0.to_string(),
            code: value.1,
        }
    }
}

impl From<String> for RemoteProtocolError {
    fn from(value: String) -> Self {
        RemoteProtocolError {
            message: value,
            code: 400,
        }
    }
}

impl From<&str> for RemoteProtocolError {
    fn from(value: &str) -> Self {
        RemoteProtocolError {
            message: value.to_string(),
            code: 400,
        }
    }
}

#[derive(Debug)]
pub enum ProtocolError {
    LocalProtocolError(LocalProtocolError),
    RemoteProtocolError(RemoteProtocolError),
}

impl From<LocalProtocolError> for ProtocolError {
    fn from(value: LocalProtocolError) -> Self {
        ProtocolError::LocalProtocolError(value)
    }
}

impl From<RemoteProtocolError> for ProtocolError {
    fn from(value: RemoteProtocolError) -> Self {
        ProtocolError::RemoteProtocolError(value)
    }
}
