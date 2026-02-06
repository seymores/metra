use anyhow::Result;
use serde::Serialize;

use crate::cli::OutputFormat;

pub fn print_output<T: Serialize>(value: &T, output: OutputFormat) -> Result<()> {
    match output {
        OutputFormat::Json | OutputFormat::Text => {
            println!("{}", serde_json::to_string_pretty(value)?);
        }
    }
    Ok(())
}
