use std::io::{self, IsTerminal, Write};

use crate::error::Error;

pub fn destructive(yes: bool, endpoint: &str, action: &str, resource: &str) -> Result<(), Error> {
    if yes {
        return Ok(());
    }
    if !io::stdin().is_terminal() {
        return Err(Error::input(
            "Destructive non-interactive commands require --yes",
        ));
    }
    eprint!("{action} {resource} at {endpoint}? Type 'yes' to continue: ");
    io::stderr()
        .flush()
        .map_err(|_| Error::input("Cannot write confirmation prompt"))?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|_| Error::input("Cannot read confirmation"))?;
    if answer.trim_end_matches(['\r', '\n']) != "yes" {
        return Err(Error::input("Operation cancelled"));
    }
    Ok(())
}
