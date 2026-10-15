//! Reading the payload a command operates on.

use std::io::Read;
use std::path::Path;

use crate::error::CliError;

/// Read `path`, or standard input when it is absent or `-`.
///
/// Payloads are read as UTF-8 with invalid sequences replaced rather than
/// rejected. Tool output is whatever the tool wrote, and a captured terminal
/// carries lone bytes from interrupted escape sequences fairly often; refusing
/// the payload would make the CLI useless on exactly the captures worth
/// inspecting, and the engine's own contract is in characters.
pub fn read_input(path: Option<&str>) -> Result<String, CliError> {
    let mut buffer = Vec::new();
    match path {
        None | Some("-") => std::io::stdin()
            .read_to_end(&mut buffer)
            .map_err(|e| CliError::runtime("reading standard input", e))?,
        Some(path) => std::fs::File::open(Path::new(path))
            .and_then(|mut file| file.read_to_end(&mut buffer))
            .map_err(|e| CliError::runtime(&format!("reading {path}"), e))?,
    };
    Ok(String::from_utf8_lossy(&buffer).into_owned())
}
