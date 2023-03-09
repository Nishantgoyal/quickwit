use std::fmt::{Display, Formatter};

use anyhow::Context;
use promptea::{BlankValidator, PromptValue, Schema};

/// The main entry point for Quickwit init.
///
/// The system will create a set of prompts for a user
/// in order to create a given config/project based on
/// the input [InitOption].
///
/// Additionally, `quiet` can be passed in order to disable
/// the prompts from displaying the additional description
/// for each field.
pub fn init(option: InitOption, quiet: bool) -> anyhow::Result<()> {
    let schema = option.as_schema();

    let input_data = schema
        .prompt(quiet)
        .context("Failed to get user input and complete init process.")?;

    let path = format!("./new-{option}-config.yaml");
    let msg = format!("Where should this config be exported? (Leave blank for {path})");
    let output_path = String::prompt(msg, Some(BlankValidator), true)
        .context("Could not get output path from user input.")?
        .unwrap_or(path);

    let exported_data =
        serde_yaml::to_string(&input_data).context("Could not serialize input data to yaml.")?;

    std::fs::write(output_path, exported_data).context("Failed to write config to file.")?;

    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
/// The selected config schema to prompt the user with.
pub enum InitOption {
    /// Create a new source config.
    Source,
    /// Create a new index config.
    Index,
    /// Create a new Quickwit config.
    Quickwit,
}

impl Display for InitOption {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            InitOption::Source => write!(f, "source"),
            InitOption::Index => write!(f, "index"),
            InitOption::Quickwit => write!(f, "quickwit"),
        }
    }
}

impl InitOption {
    fn as_schema(&self) -> Schema {
        match self {
            InitOption::Source => {
                let schema_yaml = include_str!("../schemas/source.yaml");
                serde_yaml::from_str(schema_yaml).expect("Schema should be valid yaml.")
            }
            InitOption::Index => unimplemented!("Index schema not yet implemented."),
            InitOption::Quickwit => unimplemented!("Quickwit schema not yet implemented."),
        }
    }
}
