use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct Error {
    pub kind: &'static str,
    pub message: String,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::new("io", error.to_string())
    }
}
