use std::error::Error as StdError;
use std::io::Error as IOError;
use std::fmt;

#[derive(Debug)]
pub enum ErrorKind {
    FileExists,
    DirectoryMissing,
    IO(IOError),
    Other(Box<dyn StdError>),
}

#[derive(Debug)]
pub struct Error {
    kind: ErrorKind,
}

impl Error {
    pub fn new(k: ErrorKind) -> Error {
        Error {
            kind: k,
        }
    }

    pub fn kind(&self) -> &ErrorKind {
        &self.kind
    }
}

impl From<IOError> for Error {
    fn from(err: IOError) -> Error {
        Error {
            kind: ErrorKind::IO(err),
	}
    }
}

impl From<Box<dyn StdError>> for Error {
    fn from(err: Box<dyn StdError>) -> Error {
        Error {
            kind: ErrorKind::Other(err),
	}
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.kind() {
            ErrorKind::FileExists => write!(f, "File already exists"),
            ErrorKind::DirectoryMissing => write!(f, "Destination path provided is not a valid directory"),
            ErrorKind::IO(err) => err.fmt(f),
            ErrorKind::Other(err) => err.fmt(f),
        }
    }
}
